//! TUI components

pub(crate) mod buttons;
pub(crate) mod checkbox;
mod cycler;
mod dir_picker;
mod help;
pub(crate) mod hover;
mod list_picker;
pub(crate) mod preview;
pub(crate) mod scroll;
mod text_input;

pub use cycler::{profile_cycler_spans, tool_cycler_spans};
pub use dir_picker::{DirPicker, DirPickerResult};
pub use help::HelpOverlay;
pub use list_picker::{ListPicker, ListPickerResult};
pub use preview::{format_scroll_indicator, Preview};
pub(crate) use text_input::{focused_input_spans, input_scroll, visible_slice};
pub use text_input::{
    longest_common_prefix, render_text_field, render_text_field_with_ghost,
    set_input_cursor_position, set_prefixed_input_cursor_position, GroupGhostCompletion,
};
