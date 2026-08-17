//! mana's look, in one file: the palette and the style of every element the
//! interface draws. Nothing else under `src/tui` names a colour.
//!
//! ## Why violet
//!
//! mana is "manage agent", and mana is also the old word for magic energy --
//! violet since long before terminals. So the interface wears violet: the
//! frames, the markers, the keys, and above all mana's own voice, which used
//! to speak in cyan for no reason at all.
//!
//! What the violet never touches is body text. The PM's prose keeps the
//! terminal's own foreground, because an accent on the text is a readability
//! tax paid on every line of a session that runs for an afternoon. Colour is
//! chrome here; the content is left alone.
//!
//! ## Why indexed and not truecolor
//!
//! Every colour is a 256-palette index rather than an RGB triple. Terminal.app
//! -- which is what a mac ships with -- approximates 24-bit colour by
//! quantising it back to its own palette, and not always to the neighbour the
//! author meant; the indexed palette is the one thing every terminal since
//! xterm renders the same way. The hex in each comment is the xterm reference
//! swatch: what the index looks like, not what mana sends.
//!
//! ## The contrast budget
//!
//! Ratios below are against pure black and against a #1e1e2e-ish dark theme,
//! the two backgrounds this palette is tuned for. Anything carrying words
//! clears 4.5:1 on both; only the frame lines and the separator dots -- which
//! carry no information a reader has to decode -- sit below it, on purpose.

use ratatui::style::{Color, Modifier, Style};

// ---------------------------------------------------------------- palette

/// mana's violet, and the accent of the whole interface: the identity mark,
/// mana's own notices, the caret waiting for a turn. 11.5:1 on black, 9.0:1 on
/// #1e1e2e -- bright enough that it never reads as a dimmed colour.
pub const MANA: Color = Color::Indexed(183); // #d7afff

/// A violet so pale it reads as white with a cast (15:1 / 11.8:1). Carries the
/// one thing on screen the user wrote themselves, where the accent has to be
/// visible without costing a single point of legibility.
pub const GLOW: Color = Color::Indexed(189); // #d7d7ff

/// Muted lavender (9.9:1 / 7.8:1): secondary text that is still meant to be
/// read -- the keys, the graph's nodes, the counter of what is hidden.
pub const LAVENDER: Color = Color::Indexed(146); // #afafd7

/// Deep plum (6.9:1 / 5.4:1): facts one glances at rather than reads -- which
/// CLI is driving, what the last turn cost, an empty pane saying it is empty.
pub const PLUM: Color = Color::Indexed(139); // #af87af

/// Violet-grey (6.1:1 / 4.8:1) for the technical lines behind Ctrl+O. Muted
/// enough to recede, legible enough to honour "degraded, never silent" -- it
/// is deliberately *more* readable than the grey it replaces, because a
/// degradation nobody can read is a degradation nobody notices.
pub const MIST: Color = Color::Indexed(103); // #8787af

/// The separator dots between status segments (4.0:1 / 3.1:1). Punctuation:
/// present enough to group, quiet enough that the eye skips it.
pub const SHADOW: Color = Color::Indexed(96); // #875f87

/// The frames (3.5:1 / 2.7:1). Below the text threshold on purpose: a border
/// is a hint about where a pane ends, and one that competes with its contents
/// is a border drawn too loud.
pub const DUSK: Color = Color::Indexed(60); // #5f5f87

/// The one colour here that is not violet (11.7:1 / 9.2:1). Warm peach for the
/// single state that is waiting on a human: an agent blocked on a permission.
/// Not red -- nothing is broken, somebody just has to answer -- and warm
/// enough to stand out of a cool palette from across a desk.
pub const EMBER: Color = Color::Indexed(216); // #ffaf87

/// Whatever foreground the user set their terminal to.
///
/// Not a colour mana picks -- a colour mana declines to pick, named so that
/// declining is visible in the code (and assertable in a test) rather than
/// looking like an element somebody forgot to style. The three places that use
/// it are the ones where a brand colour would cost something: the PM's prose,
/// the line being typed, and the verdict glyphs.
pub const TERMINAL_DEFAULT: Color = Color::Reset;

/// The identity mark, permanently in the chat pane's top-left corner.
///
/// U+25C6 BLACK DIAMOND: a geometric shape every monospace font in practical
/// use carries, and the same family as the ◐/○ the graph pane already ships.
/// Its East Asian width is "ambiguous", so a terminal configured for CJK may
/// draw it double-wide and nudge the rest of the title one cell right -- which
/// is why it lives in a title and not in a column that has to line up.
pub const MARK: &str = "\u{25c6}";

// ------------------------------------------------------------------ roles

/// Pane frames.
pub const BORDER: Style = Style::new().fg(DUSK);

/// The identity mark's title -- the one accent that owns the eye.
pub const TITLE: Style = Style::new().fg(MANA).add_modifier(Modifier::BOLD);

/// Every other pane label ("Graph", "You"). Subordinate by design: two shouted
/// titles and the mark stops being a mark.
pub const TITLE_MINOR: Style = Style::new().fg(LAVENDER);

/// The PM's prose: deliberately uncoloured, so it renders in whatever
/// foreground the user chose for their terminal. This is the one style in the
/// file whose job is to stay out of the way -- see the module note on why the
/// violet stops at the chrome.
pub const PM_TEXT: Style = Style::new().fg(TERMINAL_DEFAULT);

/// A turn the user typed, echoed back into the transcript.
pub const USER_TEXT: Style = Style::new().fg(GLOW).add_modifier(Modifier::BOLD);

/// mana speaking as itself. Was cyan, which belonged to no part of the
/// product; mana's voice wears mana's colour.
pub const MANA_TEXT: Style = Style::new().fg(MANA);

/// The line the PM is blocked on, in the transcript.
pub const PERMISSION_TEXT: Style = Style::new().fg(EMBER).add_modifier(Modifier::BOLD);

/// The gutter marker of a technical line. A shade brighter than the line it
/// marks: the mark says "machinery", the machinery itself recedes.
pub const RAW_MARK: Style = Style::new().fg(MIST);

/// A technical line, once Ctrl+O has opened them.
pub const RAW_TEXT: Style = Style::new().fg(MIST).add_modifier(Modifier::DIM);

/// The one line standing in for all the collapsed machinery. Quieter than the
/// conversation, louder than the lines it counts: it is an offer to open them,
/// not a piece of degraded output.
pub const COUNTER: Style = Style::new().fg(LAVENDER);

/// A key and what it does, in the status bar.
pub const STATUS_KEY: Style = Style::new().fg(LAVENDER);

/// A toggle that is currently on -- accented, because the pane is showing
/// something it does not show by default.
pub const STATUS_ON: Style = Style::new().fg(MANA).add_modifier(Modifier::BOLD);

/// A toggle in its resting state.
pub const STATUS_OFF: Style = Style::new().fg(PLUM);

/// Figures and names in the status bar: true, and nobody has to act on them.
pub const STATUS_FACT: Style = Style::new().fg(PLUM);

/// The `·` between status segments.
pub const STATUS_SEPARATOR: Style = Style::new().fg(SHADOW);

/// The status bar while an agent waits on an answer, in place of everything
/// else on the line.
pub const STATUS_ALERT: Style = Style::new().fg(EMBER).add_modifier(Modifier::BOLD);

/// The `›` in front of the input line.
pub const INPUT_CARET: Style = Style::new().fg(MANA).add_modifier(Modifier::BOLD);

/// What the user is typing: uncoloured, like the PM's prose and for the same
/// reason.
pub const INPUT_TEXT: Style = Style::new().fg(TERMINAL_DEFAULT);

/// One dispatch in the graph pane.
pub const GRAPH_NODE: Style = Style::new().fg(LAVENDER);

/// A graph pane with nothing in it yet.
pub const GRAPH_EMPTY: Style = Style::new().fg(PLUM);

/// The ✓/✗ a reviewer's verdict puts on a node: the only semantic colours in
/// the interface -- pass and fail, green and red, understood without a
/// legend. Pastel like everything else (108 is the green sibling of the
/// violet family, 174 the rose one), but never violet: the answer to "did it
/// work?" must not be a matter of brand.
pub const VERDICT_OK: Style = Style::new().fg(Color::Indexed(108));
pub const VERDICT_FAIL: Style = Style::new().fg(Color::Indexed(174));

#[cfg(test)]
mod tests {
    use super::*;

    /// Indexed, not RGB: every constant carries its xterm hex in a trailing
    /// comment, which is exactly what makes `Color::Rgb(0xd7, 0xaf, 0xff)`
    /// look like a harmless edit -- and it is not, because a terminal that
    /// quantises truecolor back down lands on a different swatch (module
    /// note). One careless edit fails this; restating each index verbatim
    /// would not have caught anything this does not.
    #[test]
    fn every_colour_is_a_palette_index() {
        for colour in [MANA, GLOW, LAVENDER, PLUM, MIST, SHADOW, DUSK, EMBER] {
            assert!(
                matches!(colour, Color::Indexed(_)),
                "{colour:?} is not a palette index"
            );
        }
    }

    /// Body text is the one thing the palette does not touch.
    #[test]
    fn the_text_the_user_reads_and_writes_keeps_the_terminals_own_foreground() {
        assert_eq!(PM_TEXT.fg, Some(TERMINAL_DEFAULT));
        assert_eq!(INPUT_TEXT.fg, Some(TERMINAL_DEFAULT));
        assert!(!matches!(TERMINAL_DEFAULT, Color::Indexed(_)));
    }

    /// Pass and fail are the interface's only semantic colours: green and
    /// rose, pastel like the family, and never violet -- if either drifted
    /// into the brand's colour, "did it work?" would lose its answer.
    #[test]
    fn the_verdict_colours_are_semantic_not_brand() {
        assert_ne!(VERDICT_OK.fg, VERDICT_FAIL.fg);
        for violet in [MANA, GLOW, LAVENDER, PLUM, MIST, SHADOW, DUSK] {
            assert_ne!(VERDICT_OK.fg, Some(violet));
            assert_ne!(VERDICT_FAIL.fg, Some(violet));
        }
    }

    /// The warning colour has to survive a redesign of the violet family: if
    /// attention ever becomes violet too, nothing on screen means "waiting".
    #[test]
    fn the_attention_colour_is_not_part_of_the_violet_family() {
        assert_ne!(EMBER, MANA);
        assert_eq!(STATUS_ALERT.fg, Some(EMBER));
        assert_eq!(PERMISSION_TEXT.fg, Some(EMBER));
    }
}
