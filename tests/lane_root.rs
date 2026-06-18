//! Fix 1: `LANE_ROOT` must resolve to an absolute path from every source.

use lane::cli::resolve_lane_root_from;
use std::path::PathBuf;

#[test]
fn relative_cli_flag_is_rejected() {
    let result = resolve_lane_root_from(Some(PathBuf::from("rel/lane")), None, None);
    assert!(result.is_err());
}

#[test]
fn relative_env_value_is_rejected() {
    let result = resolve_lane_root_from(None, Some("rel/lane".to_string()), None);
    assert!(result.is_err());
}

#[test]
fn absolute_cli_flag_is_accepted() {
    let path = resolve_lane_root_from(Some(PathBuf::from("/abs/lane")), None, None).unwrap();
    assert_eq!(path, PathBuf::from("/abs/lane"));
}

#[test]
fn env_value_is_used_when_no_flag() {
    let path = resolve_lane_root_from(None, Some("/abs/from-env".to_string()), None).unwrap();
    assert_eq!(path, PathBuf::from("/abs/from-env"));
}

#[test]
fn home_fallback_is_absolute() {
    let path = resolve_lane_root_from(None, None, Some("/Users/x".to_string())).unwrap();
    assert_eq!(path, PathBuf::from("/Users/x/.lane"));
}

#[test]
fn relative_home_is_rejected() {
    let result = resolve_lane_root_from(None, None, Some("relative-home".to_string()));
    assert!(result.is_err());
}
