use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub row: u16,
    pub col: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn new(row: u16, col: u16, width: u16, height: u16) -> Self {
        Self {
            row,
            col,
            width,
            height,
        }
    }

    pub fn bottom(&self) -> u16 {
        self.row + self.height
    }

    pub fn right(&self) -> u16 {
        self.col + self.width
    }

    pub fn contains(&self, row: u16, col: u16) -> bool {
        row >= self.row && row < self.bottom() && col >= self.col && col < self.right()
    }

    pub fn area(&self) -> u32 {
        self.width as u32 * self.height as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Constraint {
    Fixed(u16),
    Pct(u16),
    Fill,
}

#[derive(Clone, Debug)]
pub enum LayoutTree {
    Leaf {
        name: String,
        constraint: Constraint,
    },
    Split {
        direction: Direction,
        children: Vec<LayoutTree>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    NW,
    NE,
    SW,
    SE,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloatRelative {
    Editor,
    Cursor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Border {
    None,
    Single,
    Double,
    Rounded,
}

#[derive(Clone, Debug, Default)]
pub struct Gutters {
    pub pad_left: u16,
    pub pad_right: u16,
    pub scrollbar: bool,
}

pub fn resolve_layout(tree: &LayoutTree, area: Rect) -> HashMap<String, Rect> {
    let mut result = HashMap::new();
    resolve_node(tree, area, &mut result);
    result
}

fn resolve_node(node: &LayoutTree, area: Rect, out: &mut HashMap<String, Rect>) {
    match node {
        LayoutTree::Leaf { name, .. } => {
            out.insert(name.clone(), area);
        }
        LayoutTree::Split {
            direction,
            children,
        } => {
            let total = match direction {
                Direction::Vertical => area.height,
                Direction::Horizontal => area.width,
            };
            let sizes = resolve_constraints(children, total);
            let mut offset = 0u16;
            for (child, &size) in children.iter().zip(sizes.iter()) {
                let child_area = match direction {
                    Direction::Vertical => Rect::new(area.row + offset, area.col, area.width, size),
                    Direction::Horizontal => {
                        Rect::new(area.row, area.col + offset, size, area.height)
                    }
                };
                resolve_node(child, child_area, out);
                offset += size;
            }
        }
    }
}

fn resolve_constraints(children: &[LayoutTree], total: u16) -> Vec<u16> {
    let mut sizes = vec![0u16; children.len()];
    let mut remaining = total;
    let mut fill_count = 0u16;

    for (i, child) in children.iter().enumerate() {
        match constraint_of(child) {
            Constraint::Fixed(n) => {
                let n = n.min(remaining);
                sizes[i] = n;
                remaining -= n;
            }
            Constraint::Pct(pct) => {
                let n = ((total as u32 * pct as u32) / 100).min(remaining as u32) as u16;
                sizes[i] = n;
                remaining -= n;
            }
            Constraint::Fill => {
                fill_count += 1;
            }
        }
    }

    if let Some(per_fill) = remaining.checked_div(fill_count) {
        let mut extra = remaining % fill_count;
        for (i, child) in children.iter().enumerate() {
            if matches!(constraint_of(child), Constraint::Fill) {
                sizes[i] = per_fill + u16::from(extra > 0);
                extra = extra.saturating_sub(1);
            }
        }
    }

    sizes
}

fn constraint_of(node: &LayoutTree) -> Constraint {
    match node {
        LayoutTree::Leaf { constraint, .. } => *constraint,
        LayoutTree::Split { .. } => Constraint::Fill,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_leaf_fills_area() {
        let tree = LayoutTree::Leaf {
            name: "main".into(),
            constraint: Constraint::Fill,
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["main"], Rect::new(0, 0, 80, 24));
    }

    #[test]
    fn vertical_split_fixed_and_fill() {
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![
                LayoutTree::Leaf {
                    name: "top".into(),
                    constraint: Constraint::Fill,
                },
                LayoutTree::Leaf {
                    name: "bottom".into(),
                    constraint: Constraint::Fixed(5),
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["top"], Rect::new(0, 0, 80, 19));
        assert_eq!(result["bottom"], Rect::new(19, 0, 80, 5));
    }

    #[test]
    fn vertical_split_pct_and_fill() {
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![
                LayoutTree::Leaf {
                    name: "transcript".into(),
                    constraint: Constraint::Fill,
                },
                LayoutTree::Leaf {
                    name: "prompt".into(),
                    constraint: Constraint::Pct(25),
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["prompt"].height, 6); // 25% of 24
        assert_eq!(result["transcript"].height, 18);
    }

    #[test]
    fn horizontal_split() {
        let tree = LayoutTree::Split {
            direction: Direction::Horizontal,
            children: vec![
                LayoutTree::Leaf {
                    name: "left".into(),
                    constraint: Constraint::Fixed(20),
                },
                LayoutTree::Leaf {
                    name: "right".into(),
                    constraint: Constraint::Fill,
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["left"], Rect::new(0, 0, 20, 24));
        assert_eq!(result["right"], Rect::new(0, 20, 60, 24));
    }

    #[test]
    fn multiple_fills_distribute_evenly() {
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![
                LayoutTree::Leaf {
                    name: "a".into(),
                    constraint: Constraint::Fill,
                },
                LayoutTree::Leaf {
                    name: "b".into(),
                    constraint: Constraint::Fill,
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["a"].height, 12);
        assert_eq!(result["b"].height, 12);
    }

    #[test]
    fn rect_contains() {
        let r = Rect::new(5, 10, 20, 10);
        assert!(r.contains(5, 10));
        assert!(r.contains(14, 29));
        assert!(!r.contains(15, 10));
        assert!(!r.contains(5, 30));
    }

    #[test]
    fn nested_split() {
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![
                LayoutTree::Split {
                    direction: Direction::Horizontal,
                    children: vec![
                        LayoutTree::Leaf {
                            name: "tl".into(),
                            constraint: Constraint::Fill,
                        },
                        LayoutTree::Leaf {
                            name: "tr".into(),
                            constraint: Constraint::Fill,
                        },
                    ],
                },
                LayoutTree::Leaf {
                    name: "bottom".into(),
                    constraint: Constraint::Fixed(4),
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["bottom"], Rect::new(20, 0, 80, 4));
        assert_eq!(result["tl"], Rect::new(0, 0, 40, 20));
        assert_eq!(result["tr"], Rect::new(0, 40, 40, 20));
    }

    #[test]
    fn zero_height_produces_empty_rects() {
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![
                LayoutTree::Leaf {
                    name: "a".into(),
                    constraint: Constraint::Fixed(30),
                },
                LayoutTree::Leaf {
                    name: "b".into(),
                    constraint: Constraint::Fill,
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 10));
        assert_eq!(result["a"].height, 10);
        assert_eq!(result["b"].height, 0);
    }
}
