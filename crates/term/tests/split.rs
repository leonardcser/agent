use smelt_term::{
    Axis, Border, Color, DividerStyles, Grid, LayoutStyle, LayoutTree, NoopSizer, PaintId, Rect,
    Split, SplitInteraction, SplitOptions, SplitPane, SplitPhase, SplitResizeMode, SplitSize,
    Style, Surface, Theme,
};
use std::sync::Arc;

const FIRST: PaintId = PaintId(1);
const SECOND: PaintId = PaintId(2);

fn tree(split: &Split) -> LayoutTree {
    LayoutTree::split(
        split.clone(),
        LayoutTree::leaf(FIRST),
        LayoutTree::leaf(SECOND),
    )
}

#[test]
fn fixed_cells_and_proportional_preferences_are_distinct_and_restorable() {
    for mode in [SplitResizeMode::Cells, SplitResizeMode::Proportional] {
        let split = Split::new(
            Axis::Horizontal,
            SplitOptions {
                size: SplitSize::Cells(30),
                minimum: [20, 20],
                resize_mode: mode,
                ..SplitOptions::default()
            },
        );
        let layout = tree(&split);
        let resolved = layout.resolve(Rect::new(0, 0, 101, 20), &NoopSizer);
        assert!(resolved
            .split(split.id())
            .unwrap()
            .resize(SplitPane::First, 10));
        let saved = split.preferred_size();
        assert_eq!(
            layout
                .resolve(Rect::new(0, 0, 31, 20), &NoopSizer)
                .leaf_rect(FIRST)
                .unwrap()
                .width,
            15
        );
        let wide = layout.resolve(Rect::new(0, 0, 201, 20), &NoopSizer);
        assert_eq!(
            wide.leaf_rect(FIRST).unwrap().width,
            if mode == SplitResizeMode::Cells {
                40
            } else {
                80
            }
        );
        assert_eq!(split.preferred_size(), saved);
        assert!(split.reset());
        assert_eq!(split.preferred_size(), SplitSize::Cells(30));
        assert!(split.set_preferred_size(saved));
        assert!(!split.set_preferred_size(saved));
        let restored = Split::new(
            Axis::Horizontal,
            SplitOptions {
                size: saved,
                minimum: [20, 20],
                ..SplitOptions::default()
            },
        );
        assert_ne!(split.id(), restored.id());
        assert_eq!(
            tree(&restored)
                .resolve(Rect::new(0, 0, 201, 20), &NoopSizer)
                .leaf_rect(FIRST),
            wide.leaf_rect(FIRST)
        );
    }
    assert!(SplitSize::ratio(1, 0).is_none());
    assert!(SplitSize::ratio(2, 1).is_none());
}

#[test]
fn retained_handles_share_immutable_configuration_and_all_user_sizes() {
    let split = Split::new(
        Axis::Horizontal,
        SplitOptions {
            size: SplitSize::Cells(30),
            minimum: [10, 10],
            resize_mode: SplitResizeMode::Cells,
            ..SplitOptions::default()
        },
    );
    let clone = split.clone();
    let mut options = clone.options();
    options.resize_mode = SplitResizeMode::Proportional;
    let independent = Split::new(clone.axis(), options);
    assert_eq!(clone.id(), split.id());
    assert_ne!(independent.id(), split.id());
    assert_eq!(clone.options().resize_mode, SplitResizeMode::Cells);
    let layout = tree(&clone);
    let resolved = layout.resolve(Rect::new(0, 0, 101, 10), &NoopSizer);
    assert!(resolved.split(split.id()).unwrap().equalize());
    assert_eq!(split.preferred_size(), SplitSize::Cells(50));
    assert_eq!(independent.preferred_size(), SplitSize::Cells(30));
    assert_eq!(
        layout
            .resolve(Rect::new(0, 0, 201, 10), &NoopSizer)
            .leaf_rect(FIRST)
            .unwrap()
            .width,
        50
    );
    split.set_preferred_size(SplitSize::Cells(60));
    assert_eq!(layout.natural_size((201, 10)).0, 71);
}

#[test]
fn release_applies_the_last_pointer_position_and_cancel_is_explicit() {
    let split = Split::new(
        Axis::Horizontal,
        SplitOptions {
            size: SplitSize::Cells(30),
            resize_mode: SplitResizeMode::Cells,
            ..SplitOptions::default()
        },
    );
    let layout = tree(&split);
    let resolved = layout.resolve(Rect::new(0, 0, 101, 10), &NoopSizer);
    let target = resolved.split(split.id());
    let mut interaction = SplitInteraction::default();
    let start = interaction.begin(target.unwrap(), 2, 30).unwrap();
    assert_eq!(start.phase, SplitPhase::Started);
    let end = interaction.release(target, 2, 47).unwrap();
    assert_eq!(end.phase, SplitPhase::Finished);
    assert!(end.changed);
    assert_eq!(split.preferred_size(), SplitSize::Cells(47));
    let resolved = layout.resolve(Rect::new(0, 0, 101, 10), &NoopSizer);
    let target = resolved.split(split.id());
    assert_eq!(
        interaction.begin(target.unwrap(), 2, 47).unwrap().phase,
        SplitPhase::Started
    );
    assert!(interaction.update(target, 2, 52).unwrap().changed);
    assert!(interaction.update(target, 2, 52).unwrap().changed);
    let cancelled = interaction.cancel().unwrap();
    assert_eq!(cancelled.phase, SplitPhase::Cancelled);
    assert!(cancelled.ended() && cancelled.changed);
    assert_eq!(split.preferred_size(), SplitSize::Cells(52));
    assert!(interaction.active_id().is_none());
}

#[test]
fn deep_and_wide_layouts_route_leaves_without_descendant_copies() {
    let mut layout = LayoutTree::leaf(FIRST);
    let mut handles = Vec::new();
    for index in 2..=2048 {
        let axis = if index % 2 == 0 {
            Axis::Horizontal
        } else {
            Axis::Vertical
        };
        let split = Split::new(axis, SplitOptions::default());
        layout = LayoutTree::split(split.clone(), layout, LayoutTree::leaf(PaintId(index)));
        handles.push(split);
    }
    assert_eq!(
        layout.splits().map(Split::id).collect::<Vec<_>>(),
        handles.iter().rev().map(Split::id).collect::<Vec<_>>()
    );
    let resolved = layout.resolve(Rect::new(0, 0, 200, 80), &NoopSizer);
    assert_eq!(resolved.leaves().count(), 2048);
    assert_eq!(resolved.splits().count(), 2047);
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let (target, pane) = resolved.split_for_leaf(FIRST, axis).unwrap();
        let first = handles.iter().find(|split| split.axis() == axis).unwrap();
        assert_eq!(target.split.id(), first.id());
        assert_eq!(pane, SplitPane::First);
    }
    let wide = LayoutTree::hbox(
        (1..=2048)
            .map(|index| {
                (
                    smelt_term::Constraint::Fill,
                    LayoutTree::leaf(PaintId(index)),
                )
            })
            .collect(),
    );
    let resolved = wide.resolve(Rect::new(0, 0, 4096, 10), &NoopSizer);
    assert_eq!(resolved.leaves().count(), 2048);
    assert_eq!(resolved.leaf_rect(PaintId(2048)).unwrap().left, 4094);
}

#[test]
fn no_op_gestures_do_not_overwrite_a_clamped_preference() {
    let split = Split::new(
        Axis::Horizontal,
        SplitOptions {
            size: SplitSize::Cells(60),
            minimum: [20, 20],
            ..SplitOptions::default()
        },
    );
    let layout = tree(&split);
    let resolved = layout.resolve(Rect::new(0, 0, 31, 8), &NoopSizer);
    let divider = resolved.divider_at(2, 15).unwrap();
    let mut interaction = SplitInteraction::default();
    assert!(interaction.begin(divider, 2, 14).is_none());
    assert!(interaction.begin(divider, 2, 15).is_some());
    assert!(!interaction.update(Some(divider), 2, 0).unwrap().changed);
    let end = interaction.release(Some(divider), 2, 0).unwrap();
    assert_eq!(end.phase, SplitPhase::Finished);
    assert!(!end.changed);
    assert_eq!(split.preferred_size(), SplitSize::Cells(60));
    assert_eq!(interaction.active_id(), None);
    assert!(interaction.release(None, 0, 0).is_none());
}

#[test]
fn nested_drag_uses_current_geometry_and_reports_gesture_completion() {
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let split = Split::new(
            axis,
            SplitOptions {
                size: SplitSize::Cells(30),
                minimum: [10, 10],
                resize_mode: SplitResizeMode::Cells,
                ..SplitOptions::default()
            },
        );
        let layout = LayoutTree::frame(tree(&split))
            .with_border(Border::single())
            .with_padding(2);
        let rect = Rect::new(4, 7, 107, 107);
        let resolved = layout.resolve(rect, &NoopSizer);
        let divider = resolved.split(split.id()).unwrap();
        let at = divider.divider.rect;
        let mut interaction = SplitInteraction::default();
        let start = interaction.begin(divider, at.top, at.left).unwrap();
        assert!(!start.changed && !start.ended());
        assert_eq!(interaction.active_id(), Some(split.id()));
        assert!(interaction.begin(divider, at.top, at.left).is_none());
        let (row, col) = match axis {
            Axis::Horizontal => (at.top, at.left + 9),
            Axis::Vertical => (at.top + 9, at.left),
        };
        assert!(interaction.update(Some(divider), row, col).unwrap().changed);
        let resized = layout.resolve(Rect::new(8, 11, 87, 87), &NoopSizer);
        let first = resized.split(split.id()).unwrap().geometry.panes[0];
        let (row, col) = match axis {
            Axis::Horizontal => (first.top, first.left + 45),
            Axis::Vertical => (first.top + 45, first.left),
        };
        assert!(
            interaction
                .update(resized.split(split.id()), row, col)
                .unwrap()
                .changed
        );
        assert_eq!(split.preferred_size(), SplitSize::Cells(45));
        let end = interaction
            .release(resized.split(split.id()), row, col)
            .unwrap();
        assert!(end.changed && end.ended());
        assert_eq!(end.phase, SplitPhase::Finished);
    }
}

#[test]
fn removed_split_cancels_capture_without_touching_a_replacement() {
    let split = Split::new(
        Axis::Horizontal,
        SplitOptions {
            size: SplitSize::Cells(30),
            minimum: [10, 10],
            ..SplitOptions::default()
        },
    );
    let layout = tree(&split).resolve(Rect::new(0, 0, 101, 20), &NoopSizer);
    let mut interaction = SplitInteraction::default();
    assert_eq!(
        interaction
            .begin(layout.split(split.id()).unwrap(), 2, 30)
            .unwrap()
            .phase,
        SplitPhase::Started
    );
    assert!(
        interaction
            .update(layout.split(split.id()), 2, 40)
            .unwrap()
            .changed
    );
    let replacement = Split::new(
        Axis::Horizontal,
        SplitOptions {
            size: SplitSize::Cells(50),
            minimum: [10, 10],
            ..SplitOptions::default()
        },
    );
    let layout = tree(&replacement).resolve(Rect::new(0, 0, 101, 20), &NoopSizer);
    let end = interaction
        .update(layout.split(replacement.id()), 2, 60)
        .unwrap();
    assert!(end.ended() && end.changed);
    assert_eq!(replacement.preferred_size(), SplitSize::Cells(50));
    assert_eq!(interaction.active_id(), None);
    assert!(interaction.update(None, 0, 0).is_none());
}

#[test]
fn nested_leaf_routing_resizes_only_the_nearest_matching_split() {
    let outer = Split::new(
        Axis::Horizontal,
        SplitOptions {
            size: SplitSize::Cells(70),
            minimum: [10, 10],
            ..SplitOptions::default()
        },
    );
    let inner = Split::new(
        Axis::Horizontal,
        SplitOptions {
            size: SplitSize::Cells(30),
            minimum: [10, 10],
            ..SplitOptions::default()
        },
    );
    let layout = LayoutTree::split(outer.clone(), tree(&inner), LayoutTree::leaf(PaintId(3)));
    let resolved = layout.resolve(Rect::new(0, 0, 101, 20), &NoopSizer);
    let (target, pane) = resolved.split_for_leaf(SECOND, Axis::Horizontal).unwrap();
    assert_eq!(target.split.id(), inner.id());
    assert_eq!(pane, SplitPane::Second);
    assert!(target.resize(pane, 4));
    let next = layout.resolve(Rect::new(0, 0, 101, 20), &NoopSizer);
    assert_eq!(next.leaf_rect(FIRST).unwrap().width, 26);
    assert_eq!(outer.preferred_size(), SplitSize::Cells(70));
    assert_eq!(resolved.leaf_at(2, 2), Some(FIRST));
    assert_eq!(resolved.leaf_at(2, 30), None);
    assert_eq!(resolved.divider_at(2, 30).unwrap().split.id(), inner.id());
}

#[test]
fn snapshot_painting_and_surface_callbacks_use_the_resolved_leaf_rects() {
    let split = Split::new(
        Axis::Horizontal,
        SplitOptions {
            size: SplitSize::Cells(30),
            minimum: [10, 10],
            ..SplitOptions::default()
        },
    );
    let mut surface = Surface::new(101, 8);
    surface.set_layout(tree(&split));
    let resolved = surface.resolve_layout();
    split.set_preferred_size(SplitSize::Cells(50));
    let mut painted = Vec::new();
    surface
        .render_resolved(&mut Vec::new(), &resolved, |id, slice, _| {
            painted.push((id, slice.grid_rect()));
            slice.put_str(0, 0, "界e\u{301}", Style::new());
        })
        .unwrap();
    assert_eq!(painted, resolved.leaves().collect::<Vec<_>>());
    assert_eq!(painted[0].1.width, 30);
    assert_eq!(surface.paint_rect(FIRST).unwrap().width, 50);
}

#[test]
fn divider_styles_preserve_backgrounds_and_allow_per_split_overrides() {
    let defaults = DividerStyles {
        normal: Style::new().fg(Color::Green).bg(Color::DarkBlue),
        active: Style::new().fg(Color::Yellow).bg(Color::DarkRed),
    };
    for custom in [
        None,
        Some(DividerStyles {
            normal: Style::new().fg(Color::Magenta).bg(Color::DarkGreen),
            active: Style::new().fg(Color::Cyan).bg(Color::DarkGrey),
        }),
    ] {
        let split = Split::new(
            Axis::Horizontal,
            SplitOptions {
                size: SplitSize::Cells(30),
                minimum: [10, 10],
                styles: custom,
                ..SplitOptions::default()
            },
        );
        let resolved = tree(&split).resolve(Rect::new(0, 0, 101, 8), &NoopSizer);
        for active in [false, true] {
            let style = LayoutStyle {
                dividers: defaults,
                active_split: active.then_some(split.id()),
                ..LayoutStyle::default()
            };
            let mut grid = Grid::new(101, 8);
            resolved.paint(
                &mut grid,
                &Arc::new(Theme::new()),
                (101, 8),
                style,
                &mut |_, _, _, _, _| {},
            );
            assert_eq!(grid.cell(30, 2).symbol.as_str(), "│");
            assert_eq!(
                grid.cell(30, 2).style,
                custom.unwrap_or(defaults).get(active)
            );
            let mut surface = Surface::new(101, 8);
            surface.set_layout(tree(&split));
            surface.set_layout_style(style);
            let mut output = Vec::new();
            surface.render(&mut output, |_, _, _| {}).unwrap();
            assert!(String::from_utf8(output).unwrap().contains('│'));
        }
    }
}

#[test]
fn all_terminal_extents_preserve_coverage_and_preference() {
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let split = Split::new(
            axis,
            SplitOptions {
                size: SplitSize::Cells(60),
                minimum: [20, 35],
                ..SplitOptions::default()
            },
        );
        for extent in (0..250).chain([u16::MAX]) {
            let rect = match axis {
                Axis::Horizontal => Rect::new(0, 0, extent, 8),
                Axis::Vertical => Rect::new(0, 0, 8, extent),
            };
            let resolved = tree(&split).resolve(rect, &NoopSizer);
            let geometry = resolved.split(split.id()).unwrap().geometry;
            assert_eq!(
                axis.extent(geometry.panes[0])
                    + axis.extent(geometry.panes[1])
                    + axis.extent(geometry.divider),
                extent
            );
            assert_eq!(split.preferred_size(), SplitSize::Cells(60));
        }
    }
}
