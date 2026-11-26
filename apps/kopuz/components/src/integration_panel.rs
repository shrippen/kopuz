//! An integration plugin's live panel: sections of status lines, meters, buttons, toggles, sliders and choices.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use api::{IntegrationPanel, PanelControl, PanelControlKind, PanelSection};
use gpui_kit::base::slider::{SliderEvent, SliderState};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Window, div, px,
};

use crate::format::text;
use crate::job::ProgressBar;
use crate::settings::{Choice, SliderRow, Toggle, set_slider, slider_state};
use crate::style;
use store::{Link, Notices, Query};

const METER_WIDTH: f32 = 160.;
const SLIDER_STEPS: f32 = 100.;

/// Which controls a panel draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelFilter {
    All,
    /// Only the controls the plugin marked for the player.
    Quick,
}

/// `(section key, control key, value, text)`, as the daemon takes them.
type OnAct = Rc<dyn Fn(&str, &str, f32, &str, &mut App)>;

type SliderKey = (String, String);

struct SliderSlot {
    state: Entity<SliderState>,
    _subscription: Subscription,
}

/// Pass a control's change to the plugin, then read the panels again; a refusal shows as a notice.
pub fn act_on_panel(
    id: String,
    (section, control, value, text): (String, String, f32, String),
    panels: Entity<Query<Vec<IntegrationPanel>>>,
    cx: &mut App,
) {
    let call = Link::global(cx).call(move |api| async move {
        api.act_on_integration(id, section, control, value, text)
            .await
    });
    cx.spawn(async move |cx| {
        let result = call.await;
        cx.update(|cx| {
            if let Err(error) = result {
                Notices::failed(&error, cx);
            }
            panels.update(cx, |panels, cx| panels.refresh(cx));
        });
    })
    .detach();
}

/// What the filter keeps; a section left with no control is dropped.
pub fn kept(sections: &[PanelSection], filter: PanelFilter) -> Vec<PanelSection> {
    sections
        .iter()
        .filter_map(|section| {
            let controls: Vec<PanelControl> = section
                .controls
                .iter()
                .filter(|control| filter == PanelFilter::All || control.quick)
                .cloned()
                .collect();
            (!controls.is_empty()).then(|| PanelSection {
                controls,
                ..section.clone()
            })
        })
        .collect()
}

/// The step a slider moves in; a plugin that gives none gets a hundred.
fn step_of(min: f32, max: f32, step: f32) -> f32 {
    match step > 0. {
        true => step,
        false => ((max - min) / SLIDER_STEPS).max(f32::EPSILON),
    }
}

/// A dragged value on the nearest step from `min`, inside the range.
pub fn snap(value: f32, min: f32, max: f32, step: f32) -> f32 {
    let step = step_of(min, max, step);
    let snapped = min + ((value - min) / step).round() * step;
    snapped.clamp(min, max.max(min))
}

/// A slider's value as it reads beside it: whole when the step is.
fn readout(value: f32, step: f32) -> SharedString {
    match step.fract() == 0. {
        true => format!("{value:.0}"),
        false => format!("{value:.2}"),
    }
    .into()
}

/// What a panel keeps of the sliders, so a poll does not move one under the finger.
pub struct IntegrationPanelView {
    id: SharedString,
    sections: Vec<PanelSection>,
    sliders: HashMap<SliderKey, SliderSlot>,
    held: HashSet<SliderKey>,
    on_act: OnAct,
}

impl IntegrationPanelView {
    /// `id` is the integration's, which keeps the controls' element ids apart from another panel's.
    pub fn new(
        id: impl Into<SharedString>,
        on_act: impl Fn(&str, &str, f32, &str, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            sections: Vec::new(),
            sliders: HashMap::new(),
            held: HashSet::new(),
            on_act: Rc::new(on_act),
        }
    }

    /// What the daemon shows now; redraws only when it differs from what is drawn.
    pub fn set_sections(
        &mut self,
        sections: &[PanelSection],
        filter: PanelFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sections = kept(sections, filter);
        if sections == self.sections {
            return;
        }
        self.sections = sections;
        self.sync_sliders(window, cx);
        cx.notify();
    }

    fn sync_sliders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut present = HashSet::new();
        for section in &self.sections {
            for control in &section.controls {
                let PanelControlKind::Slider { min, max, step } = control.kind else {
                    continue;
                };
                let key = (section.key.clone(), control.key.clone());
                present.insert(key.clone());
                let step = step_of(min, max, step);
                let Some(slot) = self.sliders.get(&key) else {
                    let state = cx.new(|_| slider_state(min, max, step, control.value));
                    let slot = SliderSlot {
                        _subscription: cx.subscribe(&state, {
                            let key = key.clone();
                            move |this, _, event: &SliderEvent, cx| {
                                this.on_slider(&key, (min, max, step), event, cx)
                            }
                        }),
                        state,
                    };
                    self.sliders.insert(key, slot);
                    continue;
                };
                if !self.held.contains(&key) {
                    set_slider(&slot.state, control.value, window, cx);
                }
            }
        }
        self.sliders.retain(|key, _| present.contains(key));
        self.held.retain(|key| present.contains(key));
    }

    /// The daemon persists every act, so only a release acts, not each pixel of a drag.
    fn on_slider(
        &mut self,
        key: &SliderKey,
        (min, max, step): (f32, f32, f32),
        event: &SliderEvent,
        cx: &mut Context<Self>,
    ) {
        let (value, release) = match event {
            SliderEvent::Change(value) => (value.end(), false),
            SliderEvent::Release(value) => (value.end(), true),
        };
        if !release {
            self.held.insert(key.clone());
            cx.notify();
            return;
        }
        self.held.remove(key);
        (self.on_act)(&key.0, &key.1, snap(value, min, max, step), "", cx);
    }

    fn element_id(&self, section: &PanelSection, control: &PanelControl) -> SharedString {
        format!("panel-{}-{}-{}", self.id, section.key, control.key).into()
    }

    fn control(
        &self,
        section: &PanelSection,
        control: &PanelControl,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = self.element_id(section, control);
        let act = self.on_act.clone();
        let (section_key, control_key) = (section.key.clone(), control.key.clone());
        match &control.kind {
            PanelControlKind::Status => div()
                .min_w_0()
                .text_sm()
                .text_color(style::white(style::TEXT_MUTED, cx))
                .child(SharedString::from(control.text.clone()))
                .into_any_element(),
            PanelControlKind::Meter => div()
                .w(px(METER_WIDTH))
                .child(ProgressBar::new(id, Some(control.value.clamp(0., 1.))))
                .into_any_element(),
            PanelControlKind::Button => Button::new(id)
                .custom(style::soft_button(cx.theme().foreground, cx))
                .small()
                .label(SharedString::from(control.label.clone()))
                .on_click(move |_, _, cx| act(&section_key, &control_key, 0., "", cx))
                .into_any_element(),
            PanelControlKind::Toggle => Toggle::new(id)
                .enabled(control.value >= 0.5)
                .on_change(move |on, _, cx| {
                    act(&section_key, &control_key, f32::from(u8::from(*on)), "", cx)
                })
                .into_any_element(),
            PanelControlKind::Slider { step, .. } => {
                let slot = self
                    .sliders
                    .get(&(section.key.clone(), control.key.clone()));
                let step = *step;
                match slot {
                    Some(slot) => SliderRow::new(&slot.state)
                        .readout(move |value| readout(value, step))
                        .fill(true)
                        .into_any_element(),
                    None => div().into_any_element(),
                }
            }
            PanelControlKind::Choice { options } => {
                let options = options
                    .iter()
                    .map(|option| (option.value.clone(), text(&option.label)))
                    .collect();
                Choice::new(id, control.text.clone(), options)
                    .on_change(move |value: String, cx| {
                        act(&section_key, &control_key, 0., &value, cx)
                    })
                    .into_any_element()
            }
        }
    }

    fn row(
        &self,
        section: &PanelSection,
        control: &PanelControl,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // A button carries its words itself, so no label beside it.
        let label = (control.kind != PanelControlKind::Button).then(|| {
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_sm()
                .child(SharedString::from(control.label.clone()))
        });
        let slider = matches!(control.kind, PanelControlKind::Slider { .. });
        h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .justify_between()
            .children(label)
            .child(match slider {
                true => div().flex_1().child(self.control(section, control, cx)),
                false => div().child(self.control(section, control, cx)),
            })
    }
}

impl Render for IntegrationPanelView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sections = self.sections.clone();
        v_flex()
            .w_full()
            .gap_3()
            .children(sections.iter().map(|section| {
                v_flex()
                    .w_full()
                    .gap_2()
                    .children((!section.title.is_empty()).then(|| {
                        div()
                            .text_xs()
                            .text_color(style::white(style::TEXT_MUTED, cx))
                            .child(SharedString::from(section.title.clone()))
                    }))
                    .children(
                        section
                            .controls
                            .iter()
                            .map(|control| self.row(section, control, cx).into_any_element())
                            .collect::<Vec<_>>(),
                    )
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(key: &str, quick: bool) -> PanelControl {
        PanelControl {
            key: key.into(),
            quick,
            ..PanelControl::default()
        }
    }

    fn section(key: &str, controls: Vec<PanelControl>) -> PanelSection {
        PanelSection {
            key: key.into(),
            title: String::new(),
            controls,
        }
    }

    #[test]
    fn snaps_to_step() {
        assert!((snap(0.34, 0., 1., 0.1) - 0.3).abs() < 1e-6);
        assert_eq!(snap(8., 5., 20., 5.), 10.);
        assert_eq!(snap(7.4, 5., 20., 5.), 5.);
        assert_eq!(snap(99., 0., 10., 1.), 10.);
        assert_eq!(snap(-3., 0., 10., 1.), 0.);
    }

    #[test]
    fn snaps_without_a_step() {
        assert_eq!(snap(0.504, 0., 1., 0.), 0.5);
    }

    #[test]
    fn quick_keeps_only_quick_controls() {
        let sections = vec![
            section("a", vec![control("x", false), control("y", true)]),
            section("b", vec![control("z", false)]),
        ];
        let quick = kept(&sections, PanelFilter::Quick);
        assert_eq!(quick.len(), 1);
        assert_eq!(quick[0].controls.len(), 1);
        assert_eq!(quick[0].controls[0].key, "y");
        assert_eq!(kept(&sections, PanelFilter::All), sections);
    }

    #[test]
    fn readout_is_whole_for_whole_steps() {
        assert_eq!(readout(3., 1.), SharedString::from("3"));
        assert_eq!(readout(0.25, 0.05), SharedString::from("0.25"));
    }
}
