use std::io::Write;

use vox::clone;
use vox::db;

#[test]
fn test_validate_audio_missing_file() {
    assert!(clone::validate_audio("/nonexistent/path.wav").is_err());
}

#[test]
fn test_validate_audio_bad_extension() {
    let tmp = tempfile::Builder::new().suffix(".txt").tempfile().unwrap();
    let path = tmp.path().to_string_lossy().to_string();
    assert!(clone::validate_audio(&path).is_err());
}

#[test]
fn test_validate_audio_valid_wav() {
    let tmp = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    let path = tmp.path().to_string_lossy().to_string();
    assert!(clone::validate_audio(&path).is_ok());
}

#[test]
fn test_validate_audio_valid_mp3() {
    let tmp = tempfile::Builder::new().suffix(".mp3").tempfile().unwrap();
    let path = tmp.path().to_string_lossy().to_string();
    assert!(clone::validate_audio(&path).is_ok());
}

#[test]
fn test_validate_audio_valid_flac() {
    let tmp = tempfile::Builder::new().suffix(".flac").tempfile().unwrap();
    let path = tmp.path().to_string_lossy().to_string();
    assert!(clone::validate_audio(&path).is_ok());
}

#[test]
fn test_build_record_command() {
    let cmd = clone::build_record_command("/tmp/out.wav", 10);
    assert_eq!(cmd.get_program(), "rec");
    let args: Vec<_> = cmd.get_args().collect();
    assert_eq!(args, &["/tmp/out.wav", "trim", "0", "10"]);
}

#[test]
fn test_build_record_command_custom_duration() {
    let cmd = clone::build_record_command("/tmp/out.wav", 30);
    let args: Vec<_> = cmd.get_args().collect();
    assert_eq!(args, &["/tmp/out.wav", "trim", "0", "30"]);
}

#[test]
fn test_resolve_voice_found() {
    let conn = db::open_in_memory().unwrap();
    db::add_clone(&conn, "patrick", "/p.wav", Some("hello")).unwrap();
    let result = clone::resolve_voice(&conn, "patrick").unwrap();
    assert!(result.is_some());
    let vc = result.unwrap();
    assert_eq!(vc.name, "patrick");
    assert_eq!(vc.ref_audio, "/p.wav");
}

#[test]
fn test_resolve_voice_not_found() {
    let conn = db::open_in_memory().unwrap();
    let result = clone::resolve_voice(&conn, "nonexistent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_validate_audio_no_extension() {
    let mut tmp = tempfile::Builder::new().tempfile().unwrap();
    write!(tmp, "data").unwrap();
    let path = tmp.path().to_string_lossy().to_string();
    assert!(clone::validate_audio(&path).is_err());
}

// --- sanitize_clone_filename (review CL2 path-traversal fix) ---

#[test]
fn sanitize_strips_path_separators() {
    assert_eq!(clone::sanitize_clone_filename("evil/../etc"), "evil_.._etc");
    assert_eq!(clone::sanitize_clone_filename("a/b/c"), "a_b_c");
    assert_eq!(clone::sanitize_clone_filename("C:\\Users\\x"), "C__Users_x");
}

#[test]
fn sanitize_strips_leading_dots() {
    // Path separators become _ first, then leading dots stripped.
    assert_eq!(
        clone::sanitize_clone_filename("../../../etc/passwd"),
        "_.._.._etc_passwd"
    );
    assert_eq!(clone::sanitize_clone_filename(".hidden"), "hidden");
}

#[test]
fn sanitize_replaces_null_and_control_chars() {
    assert_eq!(clone::sanitize_clone_filename("clone\0evil"), "clone_evil");
    assert_eq!(clone::sanitize_clone_filename("line\nbreak"), "line_break");
}

#[test]
fn sanitize_preserves_normal_names() {
    assert_eq!(clone::sanitize_clone_filename("patrick"), "patrick");
    assert_eq!(clone::sanitize_clone_filename("voice_123"), "voice_123");
    assert_eq!(
        clone::sanitize_clone_filename("voix-française"),
        "voix-française"
    );
}

#[test]
fn sanitize_handles_empty_and_all_dots() {
    assert_eq!(clone::sanitize_clone_filename(""), "clone");
    assert_eq!(clone::sanitize_clone_filename("..."), "clone");
}
