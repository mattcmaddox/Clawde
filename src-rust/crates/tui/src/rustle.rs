//! Rustle mascot rendering for ratatui.
//!
//! An 8-row Unicode block-art mascot. Call `rustle_lines()` to get
//! 8 `Line` values ready for embedding in a Paragraph.
//!
//! Animation frames live in `FRAMES`; frame 0 is the default pose.
//! Append new frames to the array to extend the animation.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// The pose / expression of the Rustle mascot.
///
/// Every variant explicitly maps to a frame index in `FRAMES` via
/// [`rustle_lines`].  The old eye-shift micro-interactions (`LookRight`,
/// `LookDown`) were removed — the new animation is purely the 6-frame
/// loading cycle via `Loading`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RustlePose {
    Default,
    /// Loading / error spinner — `frame` drives the animation.
    Loading {
        frame: u64,
    },
}

/// Mascot style: bold green foreground rgb(0, 255, 0).
fn body_style() -> Style {
    Style::default()
        .fg(Color::Rgb(0, 255, 0))
        .add_modifier(Modifier::BOLD)
}

/// Animation frames for the mascot. Each frame is 8 rows × 29 cols.
/// Frame 0 is the default (rest) pose. Append additional frames to
/// extend the animation cycle.
///
/// chars used: ▄ ▛ ▜ ▟ ▙ █ ▔ ▌ ▗ ▖ ▐ ▝ ▘ ▚
const FRAMES: [[&str; 8]; 6] = [
    // Frame 0 — Rest claw (claws drawn in, feet on the ground)
    [
        "                             ",
        "                             ",
        "                   ▄▛▜▛▜▄    ",
        "                 ▟▛▜▙▟▙▟▛▜▙  ",
        "                 █▙▟▛▔▔▜▙▟█  ",
        "                 ██▘    ▝██  ",
        "                 ▜█▖ ▗▖ ▗█▛  ",
        "                  ████████   ",
    ],
    // Frame 1 — Rest claw up-shifted + platform
    [
        "                             ",
        "                   ▄▛▜▛▜▄    ",
        "                 ▟▛▜▙▟▙▟▛▜▙  ",
        "                 █▙▟▛▔▔▜▙▟█  ",
        "                 ██▘    ▝██  ",
        "                 ▜█▖ ▗▖ ▗█▛  ",
        "                  ████████   ",
        "                  ████████   ",
    ],
    // Frame 2 — Extend + stairs (claws out, with platform)
    [
        "                  ▗ ▐  ▌ ▖   ",
        "                  ▐▄▛▜▛▜▄▌   ",
        "                 ▟▛▜▙▟▙▟▛▜▙  ",
        "                 █▙▟▛▔▔▜▙▟█  ",
        "                 ██▘    ▝██  ",
        "                 ▜█▖ ▗▖ ▗█▛  ",
        "                  ████████   ",
        "                  ████████   ",
    ],
    // Frame 3 — Extend + stairs + staircase (with platform)
    [
        "                  ▗ ▐  ▌ ▖   ",
        "    ▚  ▚          ▐▄▛▜▛▜▄▌   ",
        "  ▚  ▚  ▚        ▟▛▜▙▟▙▟▛▜▙  ",
        "   ▚  ▚  ▚       █▙▟▛▔▔▜▙▟█  ",
        " ▚  ▚  ▚         ██▘    ▝██  ",
        "  ▚  ▚  ▚        ▜█▖ ▗▖ ▗█▛  ",
        "   ▚  ▚           ████████   ",
        "                  ████████   ",
    ],
    // Frame 4 — Rest + staircase (claws drawn in, with platform)
    [
        "                             ",
        "    ▚  ▚           ▄▛▜▛▜▄    ",
        "  ▚  ▚  ▚        ▟▛▜▙▟▙▟▛▜▙  ",
        "   ▚  ▚  ▚       █▙▟▛▔▔▜▙▟█  ",
        " ▚  ▚  ▚         ██▘    ▝██  ",
        "  ▚  ▚  ▚        ▜█▖ ▗▖ ▗█▛  ",
        "   ▚  ▚           ████████   ",
        "                  ████████   ",
    ],
    // Frame 5 — Staircase down (loop back to rest)
    [
        "                             ",
        "    ▚  ▚                     ",
        "  ▚  ▚  ▚          ▄▛▜▛▜▄    ",
        "   ▚  ▚  ▚       ▟▛▜▙▟▙▟▛▜▙  ",
        " ▚  ▚  ▚         █▙▟▛▔▔▜▙▟█  ",
        "  ▚  ▚  ▚        ██▘    ▝██  ",
        "   ▚  ▚          ▜█▖ ▗▖ ▗█▛  ",
        "                  ████████   ",
    ],
];

/// Number of animation frames available.
pub const FRAME_COUNT: usize = FRAMES.len();

/// How long each frame is displayed (in milliseconds) during the loading
/// animation.  Frame 0 (rest) holds for 3 s; frames 1-5 cycle faster at
/// 1.5 s each so the gesture animation feels responsive.
///
/// These durations are duplicated in `tools/logo-editor.html` (the Rustle
/// Animation Studio seed data); the `durations_match_rustle_studio` unit
/// test fails if the two copies drift apart.
const FRAME_DURATIONS_MS: [u64; FRAMES.len()] = [3000, 1500, 2000, 2000, 1500, 1500];

/// Total duration of one full animation cycle in milliseconds.
/// (Manual sum — `.iter().sum()` is not yet stable in const context.)
const CYCLE_MS: u64 = FRAME_DURATIONS_MS[0]
    + FRAME_DURATIONS_MS[1]
    + FRAME_DURATIONS_MS[2]
    + FRAME_DURATIONS_MS[3]
    + FRAME_DURATIONS_MS[4]
    + FRAME_DURATIONS_MS[5];

/// Given the number of milliseconds elapsed since the animation started,
/// return the frame index that should be displayed.
///
/// Walks the per-frame durations, wrapping at the cycle boundary so the
/// animation loops seamlessly.
pub fn loading_frame_for_elapsed(elapsed_ms: u64) -> u64 {
    let t = elapsed_ms % CYCLE_MS;
    let mut acc = 0u64;
    for (i, &dur) in FRAME_DURATIONS_MS.iter().enumerate() {
        acc += dur;
        if t < acc {
            return i as u64;
        }
    }
    0
}

/// Owned copy of the frames + durations, used by the in-TUI rustle editor.
/// Each entry is `(frame rows as 29-char strings, duration in ms)`.
pub(crate) fn rustle_frames_owned() -> Vec<(Vec<String>, u64)> {
    FRAMES
        .iter()
        .zip(FRAME_DURATIONS_MS.iter())
        .map(|(frame, &dur)| (frame.iter().map(|row| row.to_string()).collect(), dur))
        .collect()
}

/// Returns 8 Lines representing the Rustle mascot.
///
/// Each variant maps to a frame index explicitly so the compiler flags any
/// newly-added variant that hasn't been wired up (no wildcard arm).
pub fn rustle_lines(pose: &RustlePose) -> [Line<'static>; 8] {
    let idx = match pose {
        RustlePose::Default => 0,
        RustlePose::Loading { frame } => (*frame as usize) % FRAME_COUNT,
    };

    FRAMES[idx].map(|row| Line::from(vec![Span::styled(row.to_string(), body_style())]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn all_poses_produce_8_lines() {
        let poses = [RustlePose::Default, RustlePose::Loading { frame: 0 }];
        for pose in &poses {
            let lines = rustle_lines(pose);
            assert_eq!(lines.len(), 8, "pose {:?} should produce 8 lines", pose);
        }
    }

    #[test]
    fn each_row_is_exactly_29_chars() {
        // Check every frame, not just the default pose: the renderer centers
        // the mascot assuming a fixed 29-col width, so any misaligned row
        // would jitter the animation horizontally.
        for frame_idx in 0..FRAME_COUNT {
            let lines = rustle_lines(&RustlePose::Loading {
                frame: frame_idx as u64,
            });
            for (i, line) in lines.iter().enumerate() {
                let text = line_text(line);
                assert_eq!(
                    text.chars().count(),
                    29,
                    "row {} of frame {} should be 29 chars, got {:?}",
                    i,
                    frame_idx,
                    text
                );
            }
        }
    }

    #[test]
    fn default_pose_row_0_matches_frame() {
        let lines = rustle_lines(&RustlePose::Default);
        assert_eq!(line_text(&lines[0]), "                             ");
    }

    #[test]
    fn default_pose_last_row_is_platform() {
        let lines = rustle_lines(&RustlePose::Default);
        assert_eq!(line_text(&lines[7]), "                  ████████   ");
    }

    #[test]
    fn frame_count_is_at_least_one() {
        let _ = FRAME_COUNT; // compile-time check: FRAME_COUNT >= 1
    }

    #[test]
    fn loading_pose_cycles_through_frames() {
        // Verify each frame index maps to the correct frame via modulo.
        let row0_by_frame: [&str; 6] = [
            "                             ", // frame 0 — blank row 0 (rest, centered)
            "                             ", // frame 1 — blank row 0 (rest up-shifted)
            "                  ▗ ▐  ▌ ▖   ", // frame 2 — extended fingers
            "                  ▗ ▐  ▌ ▖   ", // frame 3 — extended fingers + staircase
            "                             ", // frame 4 — blank row 0 (rest + staircase)
            "                             ", // frame 5 — blank row 0 (rest up, loop back)
        ];
        for f in 0..(FRAMES.len() * 2) + 1 {
            let lines = rustle_lines(&RustlePose::Loading { frame: f as u64 });
            let idx = f % FRAMES.len();
            assert_eq!(
                line_text(&lines[0]),
                row0_by_frame[idx],
                "frame index {} → frame {}",
                f,
                idx
            );
        }
    }

    /// Parse the per-frame `dur:` values out of the Rustle Animation Studio
    /// (`tools/logo-editor.html`). Each studio frame carries a
    /// `dur: N, // TUI FRAME_DURATIONS_MS[i]` line; scanning for those
    /// markers yields the studio's copy of the timing in frame order.
    fn studio_frame_durations_ms() -> Vec<u64> {
        const STUDIO_HTML: &str = include_str!("../../../../tools/logo-editor.html");
        let mut out = Vec::new();
        for line in STUDIO_HTML.lines() {
            if !line.contains("TUI FRAME_DURATIONS_MS") {
                continue;
            }
            let after = line
                .split_once("dur:")
                .expect("marker line should contain a dur: field")
                .1;
            let digits: String = after
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            out.push(digits.parse().expect("studio dur should be a number"));
        }
        out
    }

    #[test]
    fn durations_match_rustle_studio() {
        // The Rustle Animation Studio (tools/logo-editor.html) stores the
        // same per-frame durations in its DEFAULT_FRAMES `dur` fields.
        // Keep the two in sync: if either side changes without the other,
        // this test fails.
        let studio = studio_frame_durations_ms();
        assert_eq!(
            studio.len(),
            FRAME_DURATIONS_MS.len(),
            "studio should define one duration per frame; the studio marks \
             each one with a `dur: N, // TUI FRAME_DURATIONS_MS` line — if \
             you changed the frames in tools/logo-editor.html, mirror the \
             change in FRAME_DURATIONS_MS here"
        );
        for (i, (rust_ms, studio_ms)) in FRAME_DURATIONS_MS.iter().zip(&studio).enumerate() {
            assert_eq!(
                rust_ms, studio_ms,
                "frame {i} duration diverged from the Rustle Animation Studio \
                 (tools/logo-editor.html); keep FRAME_DURATIONS_MS and the \
                 studio `dur` fields in sync"
            );
        }
    }

    #[test]
    fn loading_frame_for_elapsed_matches_durations() {
        // Frame 0 holds for 3000 ms (0..2999 → 0, 3000 → 1)
        assert_eq!(loading_frame_for_elapsed(0), 0);
        assert_eq!(loading_frame_for_elapsed(2999), 0);
        assert_eq!(loading_frame_for_elapsed(3000), 1);
        // Frame 1 holds for 1500 ms (3000..4499 → 1, 4500 → 2)
        assert_eq!(loading_frame_for_elapsed(4499), 1);
        assert_eq!(loading_frame_for_elapsed(4500), 2);
        // Frame 2 holds for 2000 ms (4500..6499 → 2, 6500 → 3)
        assert_eq!(loading_frame_for_elapsed(6499), 2);
        assert_eq!(loading_frame_for_elapsed(6500), 3);
        // Frame 3 holds for 2000 ms (6500..8499 → 3, 8500 → 4)
        assert_eq!(loading_frame_for_elapsed(8499), 3);
        assert_eq!(loading_frame_for_elapsed(8500), 4);
        // Frame 4 holds for 1500 ms (8500..9999 → 4, 10000 → 5)
        assert_eq!(loading_frame_for_elapsed(9999), 4);
        assert_eq!(loading_frame_for_elapsed(10000), 5);
        // Frame 5 holds for 1500 ms (10000..11499 → 5, 11500 wraps to 0)
        assert_eq!(loading_frame_for_elapsed(11499), 5);
        assert_eq!(loading_frame_for_elapsed(11500), 0);
        // Second cycle starts
        assert_eq!(loading_frame_for_elapsed(14499), 0);
        assert_eq!(loading_frame_for_elapsed(14500), 1);
    }
}
