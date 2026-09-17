// panel lab: scripted experiments against the SSD1677, for measuring
// what the datasheet reading leaves ambiguous about this panel.
//
// the production paths infer the LUT index order, the VS pulse
// polarities, the factory waveform's transition table and what the
// `0x22` mode bit selects. the lab measures them: it puts a known
// state in both RAM planes, runs one waveform, holds the panel, and
// asks the operator what changed. answers and op timings go to the
// serial log (`panel:` lines); nothing is written to the card, since
// the card is harder to reach than the console.
//
// an experiment is a list of steps. each step runs one canned
// operation and carries the questions to ask about its result. steps
// that measure the panel are preceded by a clear (and, where the
// measurement reads the plane pair, a column write) so no step
// inherits the last one's damage: the ops drive pigment, and a
// sequence that does not reset between passes measures a moving
// target. the ops only touch registers the production driver already
// uses plus the custom LUT register; no OTP programming command
// (0x08, 0x2A, 0x30, 0x36, 0x39) is reachable from here.

use crate::drivers::ssd1677::{LUT_GRAYSCALE, ctrl2};

/// Emit one line of the lab's dialogue.
///
/// The dialogue goes out under the kernel `screen` target rather than
/// the calling module's own: a held panel makes the log the only place
/// a prompt can appear, and a dev `ESP_LOG` habitually lists the kernel
/// modules, not whichever distro module drives the input. Without this
/// the prompts are filtered out and the operator sees a panel changing
/// with nothing in the log saying what to look at or press.
pub fn dialogue(args: core::fmt::Arguments<'_>) {
    log::info!(target: "plump_kernel::kernel::screen", "{}", args);
}

/// one question about what the panel did
pub struct Ask {
    pub question: &'static str,
    pub answers: &'static [&'static str],
}

/// one canned panel operation
#[derive(Clone, Copy)]
pub enum LabOp {
    /// clean clear, top half black and bottom half white
    Clear,
    /// the four {RED,BW} patterns as portrait columns, both planes
    WriteColumns,
    /// custom LUT with one live row carrying `vs` in its first three
    /// phase bytes, over the planes as they stand
    LutRow { row: u8, vs: u8 },
    /// custom LUT with rows 0 and 3 live at `vs`, over the clear's own
    /// planes (black is {0,0}, white is {1,1})
    VsProbe { vs: u8 },
    /// custom LUT with rows 1 and 2 live (opposite drives when
    /// `drive`), activated with `ctrl2`
    LutActivate { ctrl2: u8, drive: bool },
    /// factory waveform over the whole panel with these 0x21 bytes,
    /// activated with `ctrl2`
    Otp { ctrl1: &'static [u8], ctrl2: u8 },
    /// reprogram the controller registers (SW_RESET + init), the only
    /// way to put back what a custom LUT pass overwrote
    Reinit,
}

impl LabOp {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Clear => "clear (top black / bottom white)",
            Self::WriteColumns => "write plane columns 00 01 10 11",
            Self::LutRow { .. } => "custom LUT, one live row",
            Self::VsProbe { .. } => "custom LUT, rows 0 and 3 live",
            Self::LutActivate { .. } => "custom LUT, rows 1 and 2 live",
            Self::Otp { .. } => "factory waveform (OTP)",
            Self::Reinit => "reinit registers (SW_RESET + init)",
        }
    }

    /// The op's parameters, for the log line: several steps share a
    /// name and the numbers are the only thing telling them apart.
    pub fn detail(&self) -> impl core::fmt::Display + '_ {
        OpDetail(self)
    }
}

struct OpDetail<'a>(&'a LabOp);

impl core::fmt::Display for OpDetail<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self.0 {
            LabOp::Clear | LabOp::WriteColumns | LabOp::Reinit => Ok(()),
            LabOp::LutRow { row, vs } => write!(f, "row={} vs=0x{:02X}", row, vs),
            LabOp::VsProbe { vs } => write!(f, "rows=0,3 vs=0x{:02X}", vs),
            LabOp::LutActivate { ctrl2, drive } => {
                write!(f, "ctrl2=0x{:02X} drive={}", ctrl2, drive)
            }
            LabOp::Otp { ctrl1, ctrl2 } => {
                write!(f, "ctrl1={:02X?} ctrl2=0x{:02X}", ctrl1, ctrl2)
            }
        }
    }
}

/// one step: an op and the questions about what it showed
pub struct LabStep {
    pub op: LabOp,
    pub asks: &'static [Ask],
}

pub struct Experiment {
    pub name: &'static str,
    pub sub: &'static str,
    pub steps: &'static [LabStep],
}

// ── the LUT blobs the ops run ───────────────────────────────────────

/// the production gray LUT with every VS entry zeroed and the given
/// rows carrying `vs` in their first three phase bytes; timing, frame
/// rate and voltages stay as the gray pass uses them
pub fn lut_with_rows(rows: &[(u8, u8)]) -> [u8; 112] {
    let mut lut = LUT_GRAYSCALE;
    for r in 0..5 {
        lut[r * 10..r * 10 + 10].fill(0);
    }
    for &(row, vs) in rows {
        let r = (row as usize).min(3) * 10;
        lut[r..r + 3].fill(vs);
    }
    lut
}

// ── questions ───────────────────────────────────────────────────────

static NO_ASKS: &[Ask] = &[];

static COLUMN_ASKS: &[Ask] = &[
    Ask {
        question: "which column moved?",
        answers: &[
            "col 1 (00)",
            "col 2 (01)",
            "col 3 (10)",
            "col 4 (11)",
            "none",
        ],
    },
    Ask {
        question: "which half moved?",
        answers: &[
            "black half lightened",
            "white half darkened",
            "both changed",
            "neither",
        ],
    },
];

static HALF_ASKS: &[Ask] = &[
    Ask {
        question: "top (black) half:",
        answers: &["unchanged", "lightened", "darkened"],
    },
    Ask {
        question: "bottom (white) half:",
        answers: &["unchanged", "lightened", "darkened"],
    },
];

static MOVE_ASKS: &[Ask] = &[Ask {
    question: "which columns changed (black <-> white)?",
    answers: &[
        "none",
        "col 1 (00)",
        "col 2 (01)",
        "col 3 (10)",
        "col 4 (11)",
        "several / all",
    ],
}];

static CUSTOM_ASKS: &[Ask] = &[Ask {
    question: "how many columns moved?",
    answers: &[
        "none moved",
        "one column moved",
        "two columns moved",
        "all columns moved",
    ],
}];

/// how clean the pass left the paper. the white half is the tell: a
/// uniform drive error shows there and not on black.
static MIST_ASKS: &[Ask] = &[Ask {
    question: "how clean is the white half?",
    answers: &["clean white", "light grey mist", "heavy grey mist"],
}];

// ── the 0x21 parameter bytes ────────────────────────────────────────

const CTRL1_NORMAL: &[u8] = &[0x00];
const CTRL1_BYPASS: &[u8] = &[0x40];
const CTRL1_INVERT: &[u8] = &[0x80];
const CTRL1_BYPASS_TWO_BYTE: &[u8] = &[0x40, 0x00];

// ── 0x22 bases the mode experiment compares ─────────────────────────
//
// the full-clear paths use bit 3 clear (`0x14` fast, `0x34` clean) and
// the DU sets it (`0x3C`). miklav found on the SSD1683 that bit 3 is
// 3-colour vs black/white rather than "mode 1 vs 2", which would make
// every full refresh on this B/W panel run the 3-colour waveform. the
// pairs below differ only in that bit.

const CTRL2_FULL_3COL: u8 = 0x14; // LUT_LOAD + DISPLAY
const CTRL2_FULL_BW: u8 = 0x1C; // + MODE_SELECT
const CTRL2_CLEAN_3COL: u8 = 0x34; // TEMP + LUT_LOAD + DISPLAY
const CTRL2_CLEAN_BW: u8 = 0x3C; // + MODE_SELECT (the DU base)

// ── the experiments ─────────────────────────────────────────────────

pub static EXPERIMENTS: &[Experiment] = &[
    Experiment {
        name: "Plane order",
        sub: "which plane is the high bit of the LUT index",
        steps: &[
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::LutRow { row: 1, vs: 0xAA },
                asks: COLUMN_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::LutRow { row: 2, vs: 0xAA },
                asks: COLUMN_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::LutRow { row: 0, vs: 0xAA },
                asks: COLUMN_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::LutRow { row: 3, vs: 0xAA },
                asks: COLUMN_ASKS,
            },
        ],
    },
    Experiment {
        name: "VS polarity",
        sub: "what each VS code does to a black half and a white half",
        steps: &[
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::VsProbe { vs: 0x00 },
                asks: HALF_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::VsProbe { vs: 0xAA },
                asks: HALF_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::VsProbe { vs: 0x54 },
                asks: HALF_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::VsProbe { vs: 0xFC },
                asks: HALF_ASKS,
            },
        ],
    },
    Experiment {
        name: "OTP transitions",
        sub: "which plane pairs the factory waveform drives",
        steps: &[
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Otp {
                    ctrl1: CTRL1_NORMAL,
                    ctrl2: ctrl2::DU,
                },
                asks: MOVE_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Otp {
                    ctrl1: CTRL1_BYPASS,
                    ctrl2: ctrl2::DU,
                },
                asks: MOVE_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Otp {
                    ctrl1: CTRL1_INVERT,
                    ctrl2: ctrl2::DU,
                },
                asks: MOVE_ASKS,
            },
        ],
    },
    Experiment {
        name: "Ctrl1 width",
        sub: "one 0x21 byte against the two the production paths send",
        steps: &[
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Otp {
                    ctrl1: CTRL1_BYPASS,
                    ctrl2: ctrl2::DU,
                },
                asks: MOVE_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Otp {
                    ctrl1: CTRL1_BYPASS_TWO_BYTE,
                    ctrl2: ctrl2::DU,
                },
                asks: MOVE_ASKS,
            },
        ],
    },
    Experiment {
        name: "Custom LUT",
        sub: "which activation actually consumes the loaded waveform",
        steps: &[
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::LutActivate {
                    ctrl2: ctrl2::CUSTOM_LUT,
                    drive: false,
                },
                asks: CUSTOM_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::LutActivate {
                    ctrl2: ctrl2::CUSTOM_LUT,
                    drive: true,
                },
                asks: CUSTOM_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::LutActivate {
                    ctrl2: 0x1C,
                    drive: true,
                },
                asks: CUSTOM_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::LutActivate {
                    ctrl2: 0xDC,
                    drive: true,
                },
                asks: CUSTOM_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::LutActivate {
                    ctrl2: 0x3C,
                    drive: true,
                },
                asks: CUSTOM_ASKS,
            },
        ],
    },
    Experiment {
        name: "Mode bit",
        sub: "0x22 bit 3: the full waveforms against their B/W twin",
        steps: &[
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Otp {
                    ctrl1: CTRL1_BYPASS,
                    ctrl2: CTRL2_CLEAN_3COL,
                },
                asks: MIST_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Otp {
                    ctrl1: CTRL1_BYPASS,
                    ctrl2: CTRL2_CLEAN_BW,
                },
                asks: MIST_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Otp {
                    ctrl1: CTRL1_BYPASS,
                    ctrl2: CTRL2_FULL_3COL,
                },
                asks: MIST_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Otp {
                    ctrl1: CTRL1_BYPASS,
                    ctrl2: CTRL2_FULL_BW,
                },
                asks: MIST_ASKS,
            },
        ],
    },
    Experiment {
        name: "Voltage poison",
        sub: "does a custom LUT pass leave the OTP voltages wrong",
        steps: &[
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::WriteColumns,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::LutActivate {
                    ctrl2: ctrl2::CUSTOM_LUT,
                    drive: true,
                },
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: MIST_ASKS,
            },
            LabStep {
                op: LabOp::Reinit,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Clear,
                asks: MIST_ASKS,
            },
        ],
    },
    Experiment {
        name: "Timing",
        sub: "waveform durations, read off the log",
        steps: &[
            LabStep {
                op: LabOp::Clear,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Otp {
                    ctrl1: CTRL1_NORMAL,
                    ctrl2: ctrl2::DU,
                },
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Otp {
                    ctrl1: CTRL1_BYPASS,
                    ctrl2: ctrl2::DU,
                },
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Otp {
                    ctrl1: CTRL1_BYPASS,
                    ctrl2: CTRL2_CLEAN_BW,
                },
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::Reinit,
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::LutRow { row: 1, vs: 0xAA },
                asks: NO_ASKS,
            },
            LabStep {
                op: LabOp::LutActivate {
                    ctrl2: 0xDC,
                    drive: true,
                },
                asks: NO_ASKS,
            },
        ],
    },
];
