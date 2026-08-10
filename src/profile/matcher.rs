// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Selects the display profile that best matches the connected Wayland outputs.

use crate::config::{Config, OutputConfig, Profile};
use crate::wayland::OutputHead;
use tracing::{debug, trace};

/// Handles matching display profiles to current hardware configuration
pub struct ProfileMatcher {
    // Future: could add caching or optimization state here
}

impl ProfileMatcher {
    /// Create a new profile matcher
    pub fn new() -> Self {
        Self {}
    }

    /// Find the best matching profile for the given heads
    pub fn find_best_match<'a>(
        &self,
        config: &'a Config,
        heads: Vec<&OutputHead>,
    ) -> Option<&'a Profile> {
        debug!(
            "Looking for profile match among {} profiles",
            config.profiles.len()
        );

        // Try each profile in order
        for profile in &config.profiles {
            if self.profile_matches(profile, &heads) {
                debug!("Profile '{}' matches current configuration", profile.name);
                return Some(profile);
            }
        }

        debug!("No matching profile found");
        None
    }

    /// Check if a profile matches the current head configuration
    fn profile_matches(&self, profile: &Profile, heads: &[&OutputHead]) -> bool {
        trace!(
            "Checking profile '{}' against {} heads",
            profile.name,
            heads.len()
        );

        resolve_profile_outputs(profile, heads).is_some()
    }

    /// Check if an output configuration matches a head
    #[cfg(test)]
    fn output_matches_head(&self, output: &OutputConfig, head: &OutputHead) -> bool {
        output_matches_head(output, head)
    }

    #[cfg(test)]
    fn glob_match(&self, pattern: &str, text: &str) -> bool {
        glob_match(pattern, text)
    }
}

/// Resolve every configured output to one distinct compositor head.
pub(crate) fn resolve_profile_outputs<'a>(
    profile: &'a Profile,
    heads: &[&OutputHead],
) -> Option<Vec<(&'a OutputConfig, String)>> {
    if profile.outputs.len() != heads.len() {
        trace!(
            "Profile '{}' has {} outputs, but {} heads connected",
            profile.name,
            profile.outputs.len(),
            heads.len()
        );
        return None;
    }

    let mut matched_heads = vec![false; heads.len()];
    let mut resolved = Vec::with_capacity(profile.outputs.len());

    for output in &profile.outputs {
        let Some((head_idx, head)) = heads.iter().enumerate().find(|(head_idx, head)| {
            !matched_heads[*head_idx] && output_matches_head(output, head)
        }) else {
            trace!("Output '{}' has no matching head", output.name);
            return None;
        };

        matched_heads[head_idx] = true;
        trace!("Output '{}' matches head '{}'", output.name, head.name);
        resolved.push((output, head.name.clone()));
    }

    Some(resolved)
}

fn output_matches_head(output: &OutputConfig, head: &OutputHead) -> bool {
    if output.name == "*" || glob_match(&output.name, &head.name) {
        return true;
    }

    let identifier = format!("{} {} {}", head.make, head.model, head.serial_number);
    glob_match(&output.name, identifier.trim())
}

/// Matches globs where `*` spans any characters and `?` spans one character.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern_chars: Vec<char> = pattern.chars().collect();
    let text_chars: Vec<char> = text.chars().collect();

    glob_match_recursive(&pattern_chars, 0, &text_chars, 0)
}

/// Recursive helper for glob matching
fn glob_match_recursive(pattern: &[char], p_idx: usize, text: &[char], t_idx: usize) -> bool {
    // Both exhausted - match!
    if p_idx >= pattern.len() && t_idx >= text.len() {
        return true;
    }

    // Pattern exhausted but text remains - no match
    if p_idx >= pattern.len() {
        return false;
    }

    // Handle wildcards
    match pattern[p_idx] {
        '*' => {
            // Try matching zero characters (skip the *)
            if glob_match_recursive(pattern, p_idx + 1, text, t_idx) {
                return true;
            }

            // Try matching one or more characters
            if t_idx < text.len() {
                // Consume one character from text, keep * in pattern
                return glob_match_recursive(pattern, p_idx, text, t_idx + 1);
            }

            false
        }
        '?' => {
            // ? must match exactly one character
            if t_idx < text.len() {
                glob_match_recursive(pattern, p_idx + 1, text, t_idx + 1)
            } else {
                false
            }
        }
        _ => {
            // Literal character - must match exactly
            if t_idx < text.len() && pattern[p_idx] == text[t_idx] {
                glob_match_recursive(pattern, p_idx + 1, text, t_idx + 1)
            } else {
                false
            }
        }
    }
}

impl Default for ProfileMatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputConfig;
    use wayland_client::protocol::wl_output;

    fn create_test_head(name: &str, make: &str, model: &str, serial: &str) -> OutputHead {
        use crate::wayland::protocols::OutputHead;

        OutputHead {
            id: 1,
            name: name.to_string(),
            description: format!("{} {}", make, model),
            physical_size: Some((600, 340)),
            position: Some((0, 0)),
            transform: wl_output::Transform::Normal,
            scale: 1.0,
            enabled: true,
            modes: vec![],
            current_mode: None,
            preferred_mode: None,
            make: make.to_string(),
            model: model.to_string(),
            serial_number: serial.to_string(),
            adaptive_sync: None,
        }
    }

    #[test]
    fn test_glob_match_exact() {
        let matcher = ProfileMatcher::new();

        assert!(matcher.glob_match("HDMI-A-1", "HDMI-A-1"));
        assert!(matcher.glob_match("eDP-1", "eDP-1"));
        assert!(!matcher.glob_match("HDMI-A-1", "HDMI-A-2"));
    }

    #[test]
    fn test_glob_match_star_wildcard() {
        let matcher = ProfileMatcher::new();

        // Star at end
        assert!(matcher.glob_match("HDMI-*", "HDMI-A-1"));
        assert!(matcher.glob_match("HDMI-*", "HDMI-A-2"));
        assert!(matcher.glob_match("HDMI-*", "HDMI-B-1"));
        assert!(!matcher.glob_match("HDMI-*", "eDP-1"));

        // Star at beginning
        assert!(matcher.glob_match("*-A-1", "HDMI-A-1"));
        assert!(matcher.glob_match("*-A-1", "DP-A-1"));
        assert!(!matcher.glob_match("*-A-1", "HDMI-A-2"));

        // Star in middle
        assert!(matcher.glob_match("HDMI-*-1", "HDMI-A-1"));
        assert!(matcher.glob_match("HDMI-*-1", "HDMI-B-1"));
        assert!(!matcher.glob_match("HDMI-*-1", "HDMI-A-2"));

        // Multiple stars
        assert!(matcher.glob_match("*LG*", "LG Electronics"));
        assert!(matcher.glob_match("*LG*", "LG"));
        assert!(matcher.glob_match("*LG*", "My LG Monitor"));
        assert!(!matcher.glob_match("*LG*", "Dell"));

        // Star matches zero characters
        assert!(matcher.glob_match("HDMI*", "HDMI"));
        assert!(matcher.glob_match("HDMI*", "HDMI-A-1"));
    }

    #[test]
    fn test_glob_match_question_wildcard() {
        let matcher = ProfileMatcher::new();

        // Single ?
        assert!(matcher.glob_match("HDMI-A-?", "HDMI-A-1"));
        assert!(matcher.glob_match("HDMI-A-?", "HDMI-A-2"));
        assert!(!matcher.glob_match("HDMI-A-?", "HDMI-A-12"));

        // Multiple ?
        assert!(matcher.glob_match("HDMI-?-?", "HDMI-A-1"));
        assert!(matcher.glob_match("HDMI-?-?", "HDMI-B-2"));
        assert!(!matcher.glob_match("HDMI-?-?", "HDMI-AA-1"));

        // Mix * and ?
        assert!(matcher.glob_match("HDMI-?-*", "HDMI-A-1"));
        assert!(matcher.glob_match("HDMI-?-*", "HDMI-B-12"));
        assert!(!matcher.glob_match("HDMI-?-*", "HDMI-AB-1"));
    }

    #[test]
    fn test_glob_match_edge_cases() {
        let matcher = ProfileMatcher::new();

        // Empty pattern and text
        assert!(matcher.glob_match("", ""));

        // Empty text
        assert!(matcher.glob_match("*", ""));
        assert!(!matcher.glob_match("?", ""));
        assert!(!matcher.glob_match("a", ""));

        // Empty pattern
        assert!(!matcher.glob_match("", "text"));

        // Only wildcards
        assert!(matcher.glob_match("*", "anything"));
        assert!(matcher.glob_match("**", "anything"));
        assert!(matcher.glob_match("***", "anything"));
    }

    #[test]
    fn test_output_matches_head_exact() {
        let matcher = ProfileMatcher::new();
        let head = create_test_head("HDMI-A-2", "LG Electronics", "27GL850", "002NTNH1B719");

        let output = OutputConfig {
            name: "HDMI-A-2".to_string(),
            enabled: true,
            mode: None,
            scale: None,
            position: None,
            transform: None,
            adaptive_sync: None,
        };

        assert!(matcher.output_matches_head(&output, &head));
    }

    #[test]
    fn test_output_matches_head_wildcard() {
        let matcher = ProfileMatcher::new();
        let head = create_test_head("HDMI-A-2", "LG Electronics", "27GL850", "002NTNH1B719");

        let output_star = OutputConfig {
            name: "*".to_string(),
            enabled: true,
            mode: None,
            scale: None,
            position: None,
            transform: None,
            adaptive_sync: None,
        };

        assert!(matcher.output_matches_head(&output_star, &head));

        let output_hdmi = OutputConfig {
            name: "HDMI-*".to_string(),
            enabled: true,
            mode: None,
            scale: None,
            position: None,
            transform: None,
            adaptive_sync: None,
        };

        assert!(matcher.output_matches_head(&output_hdmi, &head));
    }

    #[test]
    fn test_output_matches_head_identifier() {
        let matcher = ProfileMatcher::new();
        let head = create_test_head("HDMI-A-2", "LG Electronics", "27GL850", "002NTNH1B719");

        // Match by model in identifier string
        let output_lg = OutputConfig {
            name: "*27GL850*".to_string(),
            enabled: true,
            mode: None,
            scale: None,
            position: None,
            transform: None,
            adaptive_sync: None,
        };

        assert!(matcher.output_matches_head(&output_lg, &head));
    }

    #[test]
    fn test_resolve_profile_outputs_uses_concrete_names() {
        let hdmi = create_test_head("HDMI-A-2", "LG Electronics", "27GL850", "SERIAL-1");
        let display_port = create_test_head("DP-1", "Dell", "U2720Q", "SERIAL-2");
        let heads = vec![&hdmi, &display_port];
        let profile = Profile {
            name: "resolved".to_string(),
            outputs: vec![
                OutputConfig {
                    name: "*27GL850*".to_string(),
                    enabled: true,
                    mode: None,
                    scale: None,
                    position: None,
                    transform: None,
                    adaptive_sync: None,
                },
                OutputConfig {
                    name: "*".to_string(),
                    enabled: true,
                    mode: None,
                    scale: None,
                    position: None,
                    transform: None,
                    adaptive_sync: None,
                },
            ],
            exec: vec![],
            wallpaper: None,
            wallpaper_dir: None,
        };

        let resolved = resolve_profile_outputs(&profile, &heads).unwrap();
        assert_eq!(resolved[0].1, "HDMI-A-2");
        assert_eq!(resolved[1].1, "DP-1");
    }

    #[test]
    fn test_profile_matches_single_output() {
        let matcher = ProfileMatcher::new();
        let head = create_test_head("HDMI-A-2", "LG Electronics", "27GL850", "002NTNH1B719");
        let heads = vec![&head];

        let profile = Profile {
            name: "test-profile".to_string(),
            wallpaper_dir: None,
            outputs: vec![OutputConfig {
                name: "HDMI-A-2".to_string(),
                enabled: true,
                mode: None,
                scale: None,
                position: None,
                transform: None,
                adaptive_sync: None,
            }],
            exec: vec![],
            wallpaper: None,
        };

        assert!(matcher.profile_matches(&profile, &heads));
    }

    #[test]
    fn test_profile_matches_wildcard() {
        let matcher = ProfileMatcher::new();
        let head = create_test_head("HDMI-A-2", "LG Electronics", "27GL850", "002NTNH1B719");
        let heads = vec![&head];

        let profile = Profile {
            name: "wildcard-profile".to_string(),
            wallpaper_dir: None,
            outputs: vec![OutputConfig {
                name: "HDMI-*".to_string(),
                enabled: true,
                mode: None,
                scale: None,
                position: None,
                transform: None,
                adaptive_sync: None,
            }],
            exec: vec![],
            wallpaper: None,
        };

        assert!(matcher.profile_matches(&profile, &heads));
    }

    #[test]
    fn test_profile_matches_multiple_outputs() {
        let matcher = ProfileMatcher::new();
        let head1 = create_test_head("HDMI-A-2", "LG Electronics", "27GL850", "002NTNH1B719");
        let head2 = create_test_head("eDP-1", "BOE", "0x095F", "");
        let heads = vec![&head1, &head2];

        let profile = Profile {
            name: "dual-monitor".to_string(),
            wallpaper_dir: None,
            outputs: vec![
                OutputConfig {
                    name: "HDMI-A-2".to_string(),
                    enabled: true,
                    mode: None,
                    scale: None,
                    position: None,
                    transform: None,
                    adaptive_sync: None,
                },
                OutputConfig {
                    name: "eDP-1".to_string(),
                    enabled: true,
                    mode: None,
                    scale: None,
                    position: None,
                    transform: None,
                    adaptive_sync: None,
                },
            ],
            exec: vec![],
            wallpaper: None,
        };

        assert!(matcher.profile_matches(&profile, &heads));
    }

    #[test]
    fn test_profile_no_match_wrong_count() {
        let matcher = ProfileMatcher::new();
        let head = create_test_head("HDMI-A-2", "LG Electronics", "27GL850", "002NTNH1B719");
        let heads = vec![&head];

        // Profile expects 2 outputs but only 1 is connected
        let profile = Profile {
            name: "dual-monitor".to_string(),
            wallpaper_dir: None,
            outputs: vec![
                OutputConfig {
                    name: "HDMI-A-2".to_string(),
                    enabled: true,
                    mode: None,
                    scale: None,
                    position: None,
                    transform: None,
                    adaptive_sync: None,
                },
                OutputConfig {
                    name: "eDP-1".to_string(),
                    enabled: true,
                    mode: None,
                    scale: None,
                    position: None,
                    transform: None,
                    adaptive_sync: None,
                },
            ],
            exec: vec![],
            wallpaper: None,
        };

        assert!(!matcher.profile_matches(&profile, &heads));
    }

    #[test]
    fn test_profile_no_match_wrong_name() {
        let matcher = ProfileMatcher::new();
        let head = create_test_head("HDMI-A-2", "LG Electronics", "27GL850", "002NTNH1B719");
        let heads = vec![&head];

        let profile = Profile {
            name: "wrong-output".to_string(),
            wallpaper_dir: None,
            outputs: vec![OutputConfig {
                name: "DP-1".to_string(),
                enabled: true,
                mode: None,
                scale: None,
                position: None,
                transform: None,
                adaptive_sync: None,
            }],
            exec: vec![],
            wallpaper: None,
        };

        assert!(!matcher.profile_matches(&profile, &heads));
    }

    #[test]
    fn test_find_best_match() {
        let matcher = ProfileMatcher::new();
        let head = create_test_head("HDMI-A-2", "LG Electronics", "27GL850", "002NTNH1B719");
        let heads = vec![&head];

        let config = Config {
            profiles: vec![
                Profile {
                    name: "wrong-profile".to_string(),
                    wallpaper_dir: None,
                    outputs: vec![OutputConfig {
                        name: "DP-1".to_string(),
                        enabled: true,
                        mode: None,
                        scale: None,
                        position: None,
                        transform: None,
                        adaptive_sync: None,
                    }],
                    exec: vec![],
                    wallpaper: None,
                },
                Profile {
                    name: "correct-profile".to_string(),
                    wallpaper_dir: None,
                    outputs: vec![OutputConfig {
                        name: "HDMI-A-2".to_string(),
                        enabled: true,
                        mode: None,
                        scale: None,
                        position: None,
                        transform: None,
                        adaptive_sync: None,
                    }],
                    exec: vec![],
                    wallpaper: None,
                },
            ],
        };

        let result = matcher.find_best_match(&config, heads);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "correct-profile");
    }

    #[test]
    fn test_find_best_match_none() {
        let matcher = ProfileMatcher::new();
        let head = create_test_head("HDMI-A-2", "LG Electronics", "27GL850", "002NTNH1B719");
        let heads = vec![&head];

        let config = Config {
            profiles: vec![
                Profile {
                    name: "wrong-profile-1".to_string(),
                    wallpaper_dir: None,
                    outputs: vec![OutputConfig {
                        name: "DP-1".to_string(),
                        enabled: true,
                        mode: None,
                        scale: None,
                        position: None,
                        transform: None,
                        adaptive_sync: None,
                    }],
                    exec: vec![],
                    wallpaper: None,
                },
                Profile {
                    name: "wrong-profile-2".to_string(),
                    wallpaper_dir: None,
                    outputs: vec![OutputConfig {
                        name: "eDP-1".to_string(),
                        enabled: true,
                        mode: None,
                        scale: None,
                        position: None,
                        transform: None,
                        adaptive_sync: None,
                    }],
                    exec: vec![],
                    wallpaper: None,
                },
            ],
        };

        let result = matcher.find_best_match(&config, heads);
        assert!(result.is_none());
    }
}
