#[test]
fn format_commands_are_available_in_palette() {
    for id in ["edit.formatDocument", "edit.formatSelection"] {
        assert!(kwil_core_commands::in_palette(id), "{id} must have a real host");
    }
}

#[test]
fn formatter_range_arguments_round_trip_through_settings() {
    use kwil_core_commands::settings_ui::{SettingValue, SettingsDocument};
    let mut document = SettingsDocument::parse(
        "[formatters.custom]\nexecutable = 'fake'\nrange_args = ['{start_line}', '{end_line}']\n",
    )
    .unwrap();
    assert_eq!(document.settings().formatters["custom"].range_args, ["{start_line}", "{end_line}"]);
    document
        .edit_formatter("custom", "range_args", SettingValue::Strings(vec!["--range={start_byte}:{end_byte}".into()]))
        .unwrap();
    assert_eq!(document.settings().formatters["custom"].range_args, ["--range={start_byte}:{end_byte}"]);
}
