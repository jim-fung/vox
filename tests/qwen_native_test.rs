use vox::backend::TtsBackend;
use vox::backend::qwen_native::{QwenNativeBackend, parse_language, parse_speaker};

#[test]
fn test_qwen_native_name() {
    let backend = QwenNativeBackend;
    assert_eq!(backend.name(), "qwen-native");
}

#[test]
fn test_qwen_native_list_voices() {
    let backend = QwenNativeBackend;
    let voices = backend.list_voices().unwrap();
    assert!(voices.contains(&"Ryan".to_string()));
}

#[test]
fn test_qwen_native_is_available() {
    let backend = QwenNativeBackend;
    assert!(backend.is_available());
}

#[test]
fn test_parse_language_valid() {
    assert!(parse_language("en").is_ok());
    assert!(parse_language("fr").is_ok());
    assert!(parse_language("es").is_ok());
    assert!(parse_language("de").is_ok());
    assert!(parse_language("it").is_ok());
    assert!(parse_language("pt").is_ok());
    assert!(parse_language("zh").is_ok());
    assert!(parse_language("ja").is_ok());
    assert!(parse_language("ko").is_ok());
    assert!(parse_language("ru").is_ok());
}

#[test]
fn test_parse_language_unsupported() {
    assert!(parse_language("ar").is_err());
    assert!(parse_language("nl").is_err());
    assert!(parse_language("xx").is_err());
}

#[test]
fn test_parse_speaker_valid() {
    assert!(parse_speaker("Ryan").is_ok());
    assert!(parse_speaker("serena").is_ok());
    assert!(parse_speaker("UncleFu").is_ok());
    assert!(parse_speaker("ono_anna").is_ok());
}

#[test]
fn test_parse_speaker_invalid() {
    assert!(parse_speaker("unknown").is_err());
    assert!(parse_speaker("").is_err());
}
