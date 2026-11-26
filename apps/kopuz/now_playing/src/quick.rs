//! The controls running integration plugins mark for the player, in a popover off the bar.

use std::time::Duration;

use api::IntegrationPanel;
use components::format::{t, text};
use components::integration_panel::kept;
use components::{IntegrationPanelView, PanelFilter, act_on_panel, icons, lit_accent};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::{
    Anchor, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render,
    SharedString, Styled, Subscription, Task, Window, div, px,
};
use store::{Chrome, Query};

use crate::bar::utility;

const PANEL_WIDTH: gpui_kit::Pixels = px(300.);
/// How often the bar learns whether any plugin has something to show.
const IDLE_POLL: Duration = Duration::from_secs(2);
/// How often an open popover reads the plugins' meters and states.
const OPEN_POLL: Duration = Duration::from_millis(200);

#[derive(Clone)]
struct Group {
    id: String,
    name: SharedString,
    view: Entity<IntegrationPanelView>,
}

pub(crate) struct QuickControls {
    panels: Entity<Query<Vec<IntegrationPanel>>>,
    groups: Vec<Group>,
    open: bool,
    _poll: Task<()>,
    _subscription: Subscription,
}

impl QuickControls {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let panels =
            cx.new(|cx| Query::new(&[], |api| async move { api.integration_panels().await }, cx));
        let subscription =
            cx.observe_in(&panels, window, |this, _, window, cx| this.sync(window, cx));
        Self {
            panels,
            groups: Vec::new(),
            open: false,
            _poll: Self::poll(cx),
            _subscription: subscription,
        }
    }

    /// Whether some plugin has a control to show, which is when the bar draws the button.
    pub(crate) fn any(&self) -> bool {
        !self.groups.is_empty()
    }

    /// Slowly while closed, to know whether to show the button; fast while open, for the meters.
    fn poll(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(wait) = this.update(cx, |this, _| match this.open {
                    true => OPEN_POLL,
                    false => IDLE_POLL,
                }) else {
                    return;
                };
                cx.background_executor().timer(wait).await;
                let read = this.update(cx, |this, cx| this.read(cx));
                if read.is_err() {
                    return;
                }
            }
        })
    }

    fn read(&self, cx: &mut Context<Self>) {
        self.panels.update(cx, |panels, cx| panels.refresh(cx));
    }

    /// One group per plugin with a quick control, in the daemon's order; views are kept so sliders stay put.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panels = self.panels.read(cx).value().cloned().unwrap_or_default();
        let mut old = std::mem::take(&mut self.groups);
        let before: Vec<_> = old.iter().map(|g| (g.id.clone(), g.name.clone())).collect();

        for panel in panels {
            if kept(&panel.sections, PanelFilter::Quick).is_empty() {
                continue;
            }
            let group = match old.iter().position(|group| group.id == panel.id) {
                Some(at) => old.swap_remove(at),
                None => Group {
                    id: panel.id.clone(),
                    name: SharedString::default(),
                    view: Self::view(&panel.id, self.panels.clone(), cx),
                },
            };
            group.view.update(cx, |view, cx| {
                view.set_sections(&panel.sections, PanelFilter::Quick, window, cx)
            });
            self.groups.push(Group {
                name: text(&panel.name),
                ..group
            });
        }

        let now: Vec<_> = self
            .groups
            .iter()
            .map(|g| (g.id.clone(), g.name.clone()))
            .collect();
        if now == before {
            return;
        }
        if self.groups.is_empty() {
            self.open = false;
        }
        cx.notify();
    }

    fn view(
        id: &str,
        panels: Entity<Query<Vec<IntegrationPanel>>>,
        cx: &mut Context<Self>,
    ) -> Entity<IntegrationPanelView> {
        let id = id.to_string();
        cx.new(|_| {
            IntegrationPanelView::new(id.clone(), move |section, control, value, text, cx| {
                let act = (section.into(), control.into(), value, text.into());
                act_on_panel(id.clone(), act, panels.clone(), cx);
            })
        })
    }

    fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.open = open;
        // A fresh cadence, so the first fast read is not stuck behind a slow wait.
        self._poll = Self::poll(cx);
        if open {
            // The popover and the docked panel both want the right edge, so only one holds it.
            Chrome::global(cx).update(cx, |chrome, cx| chrome.set_panel(None, cx));
            self.read(cx);
        }
        cx.notify();
    }
}

fn content(groups: &[Group], cx: &mut App) -> impl IntoElement + use<> {
    v_flex()
        .w(PANEL_WIDTH)
        .gap_3()
        .children(groups.iter().map(|group| {
            v_flex()
                .gap_1()
                .child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(group.name.clone()),
                )
                .child(group.view.clone())
        }))
}

impl Render for QuickControls {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = lit_accent(window, cx);
        let scale = store::layout(cx).player.size.scale();
        let (groups, open) = (self.groups.clone(), self.open);
        let changed = cx.listener(|this, open: &bool, _, cx| this.set_open(*open, cx));

        // The popover wants a `Selectable` trigger, so the button is handed to it inside its frame.
        utility("quick-controls", icons::SLIDERS, scale)
            .lit(open)
            .accent(accent)
            .tooltip(t("integration_controls"))
            .host(move |button| {
                Popover::new("quick-controls-popover")
                    .anchor(Anchor::BottomRight)
                    .open(open)
                    .on_open_change(changed)
                    .trigger(button)
                    .content(move |_, _, cx| content(&groups, cx))
                    .into_any_element()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playhead_tests::boot;
    use api::{PanelControl, PanelSection};
    use gpui_kit::TestAppContext;

    fn panel(id: &str, quick: bool) -> IntegrationPanel {
        IntegrationPanel {
            id: id.into(),
            sections: vec![PanelSection {
                key: "s".into(),
                title: String::new(),
                controls: vec![PanelControl {
                    key: "c".into(),
                    quick,
                    ..PanelControl::default()
                }],
            }],
            ..IntegrationPanel::default()
        }
    }

    #[gpui_kit::test]
    fn the_button_shows_only_for_a_plugin_with_a_quick_control(cx: &mut TestAppContext) {
        boot(cx);
        let (quick, cx) = cx.add_window_view(QuickControls::new);
        quick.update_in(cx, |quick, window, cx| {
            let panels = vec![panel("plugin:a", false), panel("plugin:b", true)];
            quick.panels = cx.new(|cx| Query::held(&[], panels, cx));
            quick.sync(window, cx);
        });

        let ids = quick.read_with(cx, |quick, _| {
            quick
                .groups
                .iter()
                .map(|g| g.id.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(ids, ["plugin:b"]);

        quick.update_in(cx, |quick, window, cx| {
            quick.panels = cx.new(|cx| Query::held(&[], vec![panel("plugin:a", false)], cx));
            quick.sync(window, cx);
        });
        assert!(!quick.read_with(cx, |quick, _| quick.any()));
    }
}
