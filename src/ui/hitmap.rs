//! Rendered screen region -> clickable target mapping (`HitMap`).
//!
//! Rebuilt every frame; later registrations (drawn on top) win on overlap.

use ratatui::layout::{Position, Rect};

/// Toolbar & header button identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonId {
    Search,
    SendFile,
    Attachments,
    RoomInfo,
    Refresh,
    Settings,
    NewChat,
    Presence,
    CloseThread,
    ComposerTargetToggle,
    SubjectToggle,
    NarrowToggle,
    NarrowList,
    NarrowChat,
    NarrowThread,
    ImagePreview,
    HtmlPreview,
    HtmlToggleRaw,
    HtmlOpenBrowser,
    CommandPalette,
    DensityToggle,
    Help,
    FilterToggle,
    ConfirmOk,
    ConfirmCancel,
}

/// Inline quick actions shown on the selected message row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineActionKind {
    Reply,
    Thread,
    React,
    HtmlPreview,
    More,
}

/// Identifies which list a message belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageScope {
    Chat,
    ThreadRoot,
    ThreadReply,
}

/// Clickable targets on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    /// Navigation tab (`0` = Chats, `1` = Teams, `2` = Activity).
    NavTab(usize),
    /// A row in the active navigation pane list.
    NavRow(usize),
    /// Any rendered line belonging to message `msg_index`.
    Message { scope: MessageScope, msg_index: usize },
    /// Clickable inline action chip on the selected message (`Reply`, `Thread`, `React`, `More`).
    InlineAction {
        scope: MessageScope,
        msg_index: usize,
        action: InlineActionKind,
    },
    /// Clickable attachment or inline image chip.
    Attachment {
        scope: MessageScope,
        msg_index: usize,
        attach_index: usize,
    },
    /// Clickable reaction badge on a message.
    Reaction {
        scope: MessageScope,
        msg_index: usize,
        reaction_index: usize,
    },
    /// Channel post thread indicator (`↳ N replies · Enter to open`).
    OpenThread { msg_index: usize },
    /// Clickable top history banner / retry prompt (`[ ↑ Scroll up or press o / H to load older messages ]`).
    LoadOlderMessages,
    /// Header / toolbar / footer button.
    Button(ButtonId),
    /// Subject field inside composer.
    SubjectInput,
    /// Composer body area.
    Composer,
    /// Thread-anchored composer body area (in Wide layout when thread drawer is open).
    ThreadComposer,
    /// Modal / overlay outer frame (prevents clicks inside modal padding from closing the modal).
    OverlayBody,
    /// Item inside the top overlay.
    OverlayItem { index: usize },
    /// Candidate row inside the `@mention` autocomplete popup.
    MentionCandidate(usize),
}

#[derive(Debug, Clone, Default)]
pub struct HitMap {
    regions: Vec<(Rect, HitTarget)>,
}

impl HitMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.regions.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.regions.len()
    }

    /// Registers a clickable rectangle. Zero-sized areas are ignored.
    pub fn add(&mut self, area: Rect, target: HitTarget) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.regions.push((area, target));
    }

    /// Registers a single-row clickable span.
    pub fn add_line(&mut self, x: u16, row: u16, width: u16, target: HitTarget) {
        self.add(Rect::new(x, row, width, 1), target);
    }

    /// Resolves `(row, col)` to the top-most target.
    pub fn hit(&self, row: u16, col: u16) -> Option<HitTarget> {
        let position = Position::new(col, row);
        self.regions
            .iter()
            .rev()
            .find(|(area, _)| area.contains(position))
            .map(|(_, target)| *target)
    }

    pub fn region_of(&self, target: HitTarget) -> Option<Rect> {
        self.regions
            .iter()
            .rev()
            .find(|(_, item)| *item == target)
            .map(|(area, _)| *area)
    }

    pub fn regions(&self) -> impl Iterator<Item = &(Rect, HitTarget)> {
        self.regions.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_inside_and_outside() {
        let mut map = HitMap::new();
        map.add(Rect::new(2, 3, 5, 2), HitTarget::NavRow(7));

        assert_eq!(map.hit(3, 2), Some(HitTarget::NavRow(7)));
        assert_eq!(map.hit(4, 6), Some(HitTarget::NavRow(7)));
        assert_eq!(map.hit(2, 2), None);
        assert_eq!(map.hit(3, 7), None);
    }

    #[test]
    fn later_region_wins_overlap() {
        let mut map = HitMap::new();
        map.add(
            Rect::new(0, 0, 20, 4),
            HitTarget::Message {
                scope: MessageScope::Chat,
                msg_index: 1,
            },
        );
        map.add_line(
            2,
            2,
            8,
            HitTarget::Attachment {
                scope: MessageScope::Chat,
                msg_index: 1,
                attach_index: 0,
            },
        );
        map.add_line(
            12,
            2,
            5,
            HitTarget::Reaction {
                scope: MessageScope::Chat,
                msg_index: 1,
                reaction_index: 0,
            },
        );

        assert_eq!(
            map.hit(2, 3),
            Some(HitTarget::Attachment {
                scope: MessageScope::Chat,
                msg_index: 1,
                attach_index: 0,
            })
        );
        assert_eq!(
            map.hit(2, 14),
            Some(HitTarget::Reaction {
                scope: MessageScope::Chat,
                msg_index: 1,
                reaction_index: 0,
            })
        );
        assert_eq!(
            map.hit(0, 0),
            Some(HitTarget::Message {
                scope: MessageScope::Chat,
                msg_index: 1,
            })
        );
    }

    #[test]
    fn thread_indicator_and_zero_sized_regions() {
        let mut map = HitMap::new();
        map.add(Rect::new(0, 0, 0, 5), HitTarget::Composer);
        map.add(Rect::new(0, 0, 5, 0), HitTarget::Composer);
        assert!(map.is_empty());

        map.add_line(4, 10, 24, HitTarget::OpenThread { msg_index: 3 });
        assert_eq!(map.hit(10, 8), Some(HitTarget::OpenThread { msg_index: 3 }));
        assert_eq!(map.hit(10, 28), None);
    }

    #[test]
    fn load_older_messages_banner_is_clickable() {
        let mut map = HitMap::new();
        map.add_line(2, 1, 36, HitTarget::LoadOlderMessages);
        assert_eq!(map.hit(1, 10), Some(HitTarget::LoadOlderMessages));
        assert_eq!(map.hit(1, 40), None);
    }
}
