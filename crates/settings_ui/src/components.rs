mod dropdown;
mod font_picker;
mod icon_theme_picker;
mod input_field;
mod number_field;
#[cfg(feature = "ai")]
mod ollama_model_picker;
mod section_items;
mod theme_picker;

pub use dropdown::*;
pub use font_picker::font_picker;
pub use icon_theme_picker::icon_theme_picker;
pub use input_field::*;
pub use number_field::*;
#[cfg(feature = "ai")]
pub use ollama_model_picker::render_ollama_model_picker;
#[cfg(not(feature = "ai"))]
pub fn render_ollama_model_picker(
    _field: crate::SettingField<settings::OllamaModelName>,
    _file: crate::SettingsUiFile,
    _metadata: Option<&crate::SettingsFieldMetadata>,
    _title: &'static str,
    _description: &'static str,
    _window: &mut gpui::Window,
    _cx: &mut gpui::App,
) -> gpui::AnyElement {
    gpui::IntoElement::into_any_element(gpui::Empty)
}
pub use section_items::*;
pub use theme_picker::theme_picker;
