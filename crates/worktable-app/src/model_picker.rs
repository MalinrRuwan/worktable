//! Snapshot-driven model choices. `ModelInfo::group` is catalog-owned service
//! metadata (e.g. OpenCode Go / OpenCode Zen), never inferred from model names.
//! Provider and model IDs remain untouched, including any routing prefixes.
use gpui::{
    AnyElement, App, InteractiveElement, IntoElement, ParentElement, SharedString, Styled, Task,
    Window, div,
};
use gpui_component::{
    ActiveTheme, IndexPath,
    select::{SelectDelegate, SelectItem},
};
use worktable_events::ProviderInfo;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelIdentity {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ModelOption {
    identity: ModelIdentity,
    name: String,
    provider_name: String,
    group: String,
}

impl SelectItem for ModelOption {
    type Value = ModelIdentity;

    fn title(&self) -> SharedString {
        format!("{} · {}", self.group, self.name).into()
    }

    fn value(&self) -> &Self::Value {
        &self.identity
    }

    fn matches(&self, query: &str) -> bool {
        format!(
            "{} {} {} {}",
            self.provider_name, self.group, self.name, self.identity.model_id
        )
        .to_lowercase()
        .contains(&query.trim().to_lowercase())
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let selector = format!(
            "model-option:{}:{}",
            self.identity.provider_id, self.identity.model_id
        );
        div()
            .debug_selector(move || selector.clone())
            .min_w_0()
            .text_ellipsis()
            .child(self.name.clone())
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct ModelPicker {
    options: Vec<ModelOption>,
    groups: Vec<(String, Vec<ModelOption>)>,
}

impl ModelPicker {
    pub fn new(providers: &[ProviderInfo]) -> Self {
        let options = providers
            .iter()
            .filter(|provider| provider.api_key_set || provider.oauth_set)
            .flat_map(|provider| {
                provider.models.iter().map(move |model| ModelOption {
                    identity: ModelIdentity {
                        provider_id: provider.id.clone(),
                        model_id: model.id.clone(),
                    },
                    name: model.name.clone(),
                    provider_name: provider.name.clone(),
                    group: model.group.clone().unwrap_or_else(|| provider.name.clone()),
                })
            })
            .collect();
        let mut picker = Self {
            options,
            groups: Vec::new(),
        };
        picker.filter("");
        picker
    }

    fn filter(&mut self, query: &str) {
        self.groups.clear();
        for option in self.options.iter().filter(|option| option.matches(query)) {
            let index = self
                .groups
                .iter()
                .position(|(name, _)| name == &option.group)
                .unwrap_or_else(|| {
                    self.groups.push((option.group.clone(), Vec::new()));
                    self.groups.len() - 1
                });
            self.groups[index].1.push(option.clone());
        }
    }

    pub fn configured(&self, identity: &ModelIdentity) -> bool {
        self.options
            .iter()
            .any(|option| &option.identity == identity)
    }

    pub fn selection(&self, provider: Option<&str>, model: Option<&str>) -> Option<ModelIdentity> {
        let identity = ModelIdentity {
            provider_id: provider?.into(),
            model_id: model?.into(),
        };
        self.configured(&identity).then_some(identity)
    }
}

impl SelectDelegate for ModelPicker {
    type Item = ModelOption;

    fn sections_count(&self, _: &App) -> usize {
        self.groups.len()
    }

    fn items_count(&self, section: usize) -> usize {
        self.groups.get(section).map_or(0, |(_, items)| items.len())
    }

    fn item(&self, ix: IndexPath) -> Option<&Self::Item> {
        self.groups.get(ix.section)?.1.get(ix.row)
    }

    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        Self::Item: SelectItem<Value = V>,
        V: PartialEq,
    {
        self.groups
            .iter()
            .enumerate()
            .find_map(|(section, (_, items))| {
                items
                    .iter()
                    .position(|item| item.value() == value)
                    .map(|row| IndexPath::new(row).section(section))
            })
    }

    fn perform_search(&mut self, query: &str, _: &mut Window, _: &mut App) -> Task<()> {
        self.filter(query);
        Task::ready(())
    }

    fn render_section_header(
        &self,
        section: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let group = self.groups.get(section)?.0.clone();
        let selector = format!("model-group:{group}");
        Some(
            div()
                .debug_selector(move || selector.clone())
                .px_2()
                .py_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(group)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use worktable_events::ModelInfo;

    fn provider(id: &str, name: &str, configured: bool, groups: &[Option<&str>]) -> ProviderInfo {
        ProviderInfo {
            id: id.into(),
            name: name.into(),
            supports_api_key: true,
            supports_oauth: false,
            api_key_set: configured,
            oauth_set: false,
            groups: Vec::new(),
            models_loading: false,
            models_error: None,
            models: groups
                .iter()
                .enumerate()
                .map(|(index, group)| ModelInfo {
                    id: format!("model-{index}"),
                    name: "Shared model name".into(),
                    group: group.map(str::to_owned),
                    api: None,
                })
                .collect(),
        }
    }

    fn providers() -> Vec<ProviderInfo> {
        vec![
            provider(
                "opencode-go",
                "OpenCode",
                true,
                &[
                    Some("OpenCode Go"),
                    Some("OpenCode Zen"),
                    Some("OpenCode Go"),
                ],
            ),
            provider("other", "Other", true, &[None]),
        ]
    }

    #[test]
    fn grouping_preserves_snapshot_order_and_catalog_service_metadata() {
        let picker = ModelPicker::new(&providers());
        assert_eq!(
            picker
                .groups
                .iter()
                .map(|(name, items)| (name.as_str(), items.len()))
                .collect::<Vec<_>>(),
            vec![("OpenCode Go", 2), ("OpenCode Zen", 1), ("Other", 1)]
        );
        assert_eq!(picker.groups[0].1[1].identity.model_id, "model-2");
    }

    #[test]
    fn search_keeps_provider_model_identity_and_matches_service_and_id() {
        let mut picker = ModelPicker::new(&providers());
        let other = picker.selection(Some("other"), Some("model-0")).unwrap();
        let go = picker
            .selection(Some("opencode-go"), Some("model-0"))
            .unwrap();
        assert_ne!(other, go);
        picker.filter("shared MODEL");
        assert_eq!(picker.position(&other), Some(IndexPath::new(0).section(2)));
        picker.filter(" ZEN ");
        assert_eq!(picker.groups.len(), 1);
        assert_eq!(picker.groups[0].1[0].identity.model_id, "model-1");
        assert_eq!(picker.position(&other), None);
        picker.filter("model-2");
        assert_eq!(picker.groups[0].1[0].identity.model_id, "model-2");
        picker.filter("no matching model");
        assert!(picker.groups.is_empty());
        picker.filter("");
        assert_eq!(picker.position(&other), Some(IndexPath::new(0).section(2)));
    }

    #[test]
    fn removed_models_providers_and_logged_out_configuration_clear_selection() {
        let mut providers = providers();
        assert!(
            ModelPicker::new(&providers)
                .selection(Some("other"), Some("model-0"))
                .is_some()
        );
        providers[1].api_key_set = false;
        let picker = ModelPicker::new(&providers);
        assert!(picker.selection(Some("other"), Some("model-0")).is_none());
        assert_eq!(picker.groups.len(), 2);
        assert!(
            picker
                .options
                .iter()
                .all(|option| option.identity.provider_id != "other")
        );
        providers[1].oauth_set = true;
        assert!(
            ModelPicker::new(&providers)
                .selection(Some("other"), Some("model-0"))
                .is_some()
        );
        providers[1].oauth_set = false;
        providers[1].api_key_set = true;
        providers[1].models.clear();
        assert!(
            ModelPicker::new(&providers)
                .selection(Some("other"), Some("model-0"))
                .is_none()
        );
        providers.pop();
        assert!(
            ModelPicker::new(&providers)
                .selection(Some("other"), Some("model-0"))
                .is_none()
        );
    }

    #[gpui::test]
    fn native_select_resolves_identity_and_clears_removed_snapshot_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;
        use gpui_component::select::SelectState;
        cx.update(gpui_component::init);
        let cx = cx.add_empty_window();
        cx.update(|window, cx| {
            let providers = providers();
            let picker = ModelPicker::new(&providers);
            let identity = picker.selection(Some("other"), Some("model-0")).unwrap();
            let state = cx.new(|cx| SelectState::new(picker, None, window, cx).searchable(true));
            state.update(cx, |state, cx| {
                state.set_selected_value(&identity, window, cx)
            });
            assert_eq!(state.read(cx).selected_value(), Some(&identity));
            assert_eq!(
                state.read(cx).selected_index(cx),
                Some(IndexPath::new(0).section(2))
            );
            state.update(cx, |state, cx| {
                state.set_items(ModelPicker::new(&providers[..1]), window, cx);
                state.set_selected_value(&identity, window, cx);
            });
            assert_eq!(state.read(cx).selected_value(), None);
        });
    }
}
