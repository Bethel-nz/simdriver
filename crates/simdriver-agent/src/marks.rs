//! The accessibility tree → the elements an agent can act on: interactive, enabled and
//! on screen, numbered e1…eN in tree order, each with the point a tap would actually
//! reach. Later elements in tree order draw on top (a floating search bar after the
//! list it floats over), so an element is tapped where nothing after it covers it.
use serde::{Deserialize, Serialize};
use simdriver::AxNode;

/// Roles an agent acts on.
const INTERACTIVE: &[&str] = &[
    "AXButton",
    "AXCell",
    "AXCheckBox",
    "AXIncrementor",
    "AXLink",
    "AXMenuButton",
    "AXPopUpButton",
    "AXRadioButton",
    "AXSearchField",
    "AXSecureTextField",
    "AXSlider",
    "AXSwitch",
    "AXTab",
    "AXTextArea",
    "AXTextField",
];

/// Roles that take typed text (`fill`).
const TEXT_INPUT: &[&str] = &[
    "AXSearchField",
    "AXSecureTextField",
    "AXTextArea",
    "AXTextField",
];

/// Samples across an element when its centre is covered (columns × rows).
const SAMPLES: (u32, u32) = (7, 5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Nothing covers it.
    Visible,
    /// Partly covered or partly off screen; `tap` is a point that is not.
    Partial,
    /// Entirely under later elements: no tap point.
    Covered,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Element {
    /// `e1`, `e2`… in this snapshot.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The AX role without its prefix, lowercase: `button`, `textfield`, `switch`…
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(rename = "id", skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// `[x, y, width, height]` in screen points, as reported (may extend off screen).
    pub frame: [f64; 4],
    /// Where a tap lands, in screen points; `None` when covered.
    pub tap: Option<[f64; 2]>,
    pub visibility: Visibility,
}

impl Element {
    /// Same element in another snapshot: role, label and identifier all match.
    pub(crate) fn same_identity(&self, other: &Element) -> bool {
        self.role == other.role && self.label == other.label && self.identifier == other.identifier
    }

    /// The largest change of any frame edge, in points.
    pub(crate) fn moved_by(&self, other: &Element) -> f64 {
        (0..4)
            .map(|i| (self.frame[i] - other.frame[i]).abs())
            .fold(0.0, f64::max)
    }

    /// What an agent calls it: the label, else the value, else the identifier.
    pub fn name(&self) -> &str {
        self.label
            .as_deref()
            .or(self.value.as_deref())
            .or(self.identifier.as_deref())
            .unwrap_or("")
    }

    pub(crate) fn takes_text(&self) -> bool {
        TEXT_INPUT
            .iter()
            .any(|r| r[2..].eq_ignore_ascii_case(&self.role))
    }
}

/// Interactive elements of `root` on a `screen` (points), and how many more lie wholly
/// off screen (reachable by scrolling).
pub fn elements(root: &AxNode, screen: (f64, f64)) -> (Vec<Element>, usize) {
    // Preorder with subtree ends: an element's own descendants never cover it.
    let mut flat: Vec<(&AxNode, usize)> = Vec::new();
    fn walk<'a>(n: &'a AxNode, flat: &mut Vec<(&'a AxNode, usize)>) {
        let i = flat.len();
        flat.push((n, 0));
        for c in &n.children {
            walk(c, flat);
        }
        flat[i].1 = flat.len();
    }
    walk(root, &mut flat);

    let interactive: Vec<usize> = (0..flat.len())
        .filter(|&i| {
            let n = flat[i].0;
            n.enabled
                && INTERACTIVE.contains(&n.role.as_str())
                && n.frame[2] > 0.0
                && n.frame[3] > 0.0
        })
        .collect();

    let mut out = Vec::new();
    let mut offscreen = 0;
    for (k, &i) in interactive.iter().enumerate() {
        let (n, end) = flat[i];
        let Some(visible) = clip(n.frame, screen) else {
            offscreen += 1;
            continue;
        };
        let covers: Vec<[f64; 4]> = interactive[k + 1..]
            .iter()
            .filter(|&&j| j >= end)
            .map(|&j| flat[j].0.frame)
            .collect();
        let free = |p: [f64; 2]| !covers.iter().any(|c| inside(p, *c));
        let centre = [visible[0] + visible[2] / 2.0, visible[1] + visible[3] / 2.0];
        let samples = samples(visible);
        let free_count = samples.iter().filter(|p| free(**p)).count();
        let whole = visible == n.frame;
        let (tap, visibility) = if free(centre) {
            let v = if whole && free_count == samples.len() {
                Visibility::Visible
            } else {
                Visibility::Partial
            };
            (Some(centre), v)
        } else {
            let nearest = samples
                .iter()
                .filter(|p| free(**p))
                .min_by(|a, b| dist2(**a, centre).total_cmp(&dist2(**b, centre)))
                .copied();
            match nearest {
                Some(p) => (Some(p), Visibility::Partial),
                None => (None, Visibility::Covered),
            }
        };
        out.push(Element {
            reference: format!("e{}", out.len() + 1),
            role: n
                .role
                .strip_prefix("AX")
                .unwrap_or(&n.role)
                .to_ascii_lowercase(),
            label: n.label.clone(),
            value: n.value.clone(),
            placeholder: n.placeholder.clone(),
            identifier: n.identifier.clone(),
            frame: n.frame,
            tap,
            visibility,
        });
    }
    (out, offscreen)
}

/// Every label and value in the tree (for `waitFor`).
pub(crate) fn texts(root: &AxNode) -> impl Iterator<Item = &str> {
    root.descendants()
        .into_iter()
        .flat_map(|n| [n.label.as_deref(), n.value.as_deref()])
        .flatten()
}

fn clip(f: [f64; 4], (w, h): (f64, f64)) -> Option<[f64; 4]> {
    let (x0, y0) = (f[0].max(0.0), f[1].max(0.0));
    let (x1, y1) = ((f[0] + f[2]).min(w), (f[1] + f[3]).min(h));
    (x1 - x0 >= 1.0 && y1 - y0 >= 1.0).then_some([x0, y0, x1 - x0, y1 - y0])
}

fn inside(p: [f64; 2], r: [f64; 4]) -> bool {
    p[0] >= r[0] && p[0] <= r[0] + r[2] && p[1] >= r[1] && p[1] <= r[1] + r[3]
}

fn samples(r: [f64; 4]) -> Vec<[f64; 2]> {
    let (cols, rows) = SAMPLES;
    let mut out = Vec::new();
    for j in 0..rows {
        for i in 0..cols {
            out.push([
                r[0] + r[2] * (f64::from(i) + 0.5) / f64::from(cols),
                r[1] + r[3] * (f64::from(j) + 0.5) / f64::from(rows),
            ]);
        }
    }
    out
}

fn dist2(a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn node(role: &str, label: &str, frame: [f64; 4], children: Vec<AxNode>) -> AxNode {
        AxNode {
            role: role.into(),
            label: (!label.is_empty()).then(|| label.into()),
            value: None,
            placeholder: None,
            identifier: None,
            enabled: true,
            frame,
            children,
        }
    }

    /// Settings-like: a list whose last rows sit under a floating search field, one row
    /// past the bottom, one wholly off screen, and a disabled button.
    pub(crate) fn settings() -> AxNode {
        node(
            "AXApplication",
            "Settings",
            [0.0, 0.0, 400.0, 800.0],
            vec![
                node("AXHeading", "Settings", [16.0, 100.0, 120.0, 30.0], vec![]),
                node(
                    "AXGroup",
                    "",
                    [0.0, 0.0, 400.0, 800.0],
                    vec![
                        node("AXButton", "General", [16.0, 200.0, 368.0, 50.0], vec![]),
                        node(
                            "AXButton",
                            "Screen Time",
                            [16.0, 700.0, 368.0, 50.0],
                            vec![],
                        ),
                        node("AXButton", "Passcode", [16.0, 750.0, 368.0, 50.0], vec![]),
                        node("AXButton", "Privacy", [16.0, 790.0, 368.0, 50.0], vec![]),
                        node("AXButton", "Wallet", [16.0, 900.0, 368.0, 50.0], vec![]),
                        AxNode {
                            enabled: false,
                            ..node("AXButton", "Disabled", [16.0, 300.0, 368.0, 50.0], vec![])
                        },
                    ],
                ),
                node("AXTextField", "Search", [8.0, 725.0, 384.0, 75.0], vec![]),
            ],
        )
    }

    #[test]
    fn numbers_visible_interactive_elements_in_tree_order() {
        let (els, off) = elements(&settings(), (400.0, 800.0));
        let names: Vec<(&str, &str)> = els
            .iter()
            .map(|e| (e.reference.as_str(), e.name()))
            .collect();
        assert_eq!(
            names,
            [
                ("e1", "General"),
                ("e2", "Screen Time"),
                ("e3", "Passcode"),
                ("e4", "Privacy"),
                ("e5", "Search")
            ]
        );
        assert_eq!(off, 1, "Wallet is wholly off screen");
        assert_eq!(els[0].role, "button");
        assert_eq!(els[4].role, "textfield");
        assert!(els[4].takes_text());
    }

    #[test]
    fn covered_elements_get_a_free_tap_point_or_none() {
        let (els, _) = elements(&settings(), (400.0, 800.0));
        assert_eq!(els[0].visibility, Visibility::Visible);
        assert_eq!(els[0].tap, Some([200.0, 225.0]));
        // Screen Time: the search field covers its lower half (y 725..750), centre y 725 is
        // on the edge → covered; the tap moves up into the free part.
        let st = &els[1];
        assert_eq!(st.visibility, Visibility::Partial);
        let t = st.tap.unwrap();
        assert!(t[1] < 725.0 && t[1] >= 700.0, "{t:?}");
        // Passcode (750..800) is wholly under the field.
        assert_eq!(els[2].visibility, Visibility::Covered);
        assert_eq!(els[2].tap, None);
        // Privacy: clipped at the bottom and under the field.
        assert_eq!(els[3].visibility, Visibility::Covered);
        // The field itself is on top.
        assert_eq!(els[4].visibility, Visibility::Visible);
    }

    #[test]
    fn descendants_do_not_cover_their_container() {
        let cell = node(
            "AXCell",
            "Row",
            [0.0, 0.0, 100.0, 50.0],
            vec![node("AXButton", "Info", [0.0, 0.0, 100.0, 50.0], vec![])],
        );
        let root = node("AXApplication", "", [0.0, 0.0, 100.0, 100.0], vec![cell]);
        let (els, _) = elements(&root, (100.0, 100.0));
        assert_eq!(
            els[0].visibility,
            Visibility::Visible,
            "the cell is not covered by its own button"
        );
    }
}
