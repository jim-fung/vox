use vox::config::normalize_locale;

#[test]
fn normalize_locale_supported_variants() {
    assert_eq!(normalize_locale("en-US"), Some("en".to_string()));
    assert_eq!(normalize_locale("fr_FR"), Some("fr".to_string()));
    assert_eq!(normalize_locale("PT-br"), Some("pt".to_string()));
}

#[test]
fn normalize_locale_rejects_unsupported_languages() {
    assert_eq!(normalize_locale("sv-SE"), None);
    assert_eq!(normalize_locale(""), None);
}
