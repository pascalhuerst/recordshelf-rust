//! Single-key commands for working through releases, and the decisions behind
//! them. Kept free of UI code so the rules (e.g. when to ask before adding a
//! duplicate) can be tested.

use eframe::egui::Key;

use crate::discogs::CollectionInstance;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Add,
    Remove,
    Open,
    ChooseFolder,
    Quit,
}

impl Command {
    pub const ALL: [Command; 5] = [
        Command::Add,
        Command::Remove,
        Command::Open,
        Command::ChooseFolder,
        Command::Quit,
    ];

    pub fn key(self) -> Key {
        match self {
            Command::Add => Key::A,
            Command::Remove => Key::R,
            Command::Open => Key::O,
            Command::ChooseFolder => Key::F,
            Command::Quit => Key::Q,
        }
    }

    /// Whether the command acts on the selected release.
    pub fn needs_selection(self) -> bool {
        !matches!(self, Command::ChooseFolder | Command::Quit)
    }

    /// Key as shown to the user.
    pub fn key_label(self) -> &'static str {
        match self {
            Command::Add => "A",
            Command::Remove => "R",
            Command::Open => "O",
            Command::ChooseFolder => "F",
            Command::Quit => "Q",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AddDecision {
    /// Not in the collection: add right away.
    Add,
    /// Already owned (this many copies): ask before adding another.
    ConfirmDuplicate(usize),
    /// Collection status not loaded yet; adding blindly could create a duplicate.
    NeedsCheck,
}

/// Copies added optimistically (before Discogs confirmed them) carry this id.
pub const PENDING_INSTANCE: u32 = 0;

pub fn decide_add(owned: Option<&[CollectionInstance]>) -> AddDecision {
    match owned {
        None => AddDecision::NeedsCheck,
        Some([]) => AddDecision::Add,
        Some(copies) => AddDecision::ConfirmDuplicate(copies.len()),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemoveDecision {
    /// Ask for confirmation, then remove this copy.
    Confirm(CollectionInstance),
    NotOwned,
    /// Collection status unknown or still changing; try again in a moment.
    NeedsCheck,
}

/// Picks the copy to remove: the last one listed. Copies that were just added
/// and not yet confirmed by Discogs cannot be removed.
pub fn decide_remove(owned: Option<&[CollectionInstance]>) -> RemoveDecision {
    match owned {
        None => RemoveDecision::NeedsCheck,
        Some([]) => RemoveDecision::NotOwned,
        Some(copies) => copies
            .iter()
            .rfind(|c| c.instance_id != PENDING_INSTANCE)
            .map_or(RemoveDecision::NeedsCheck, |c| {
                RemoveDecision::Confirm(c.clone())
            }),
    }
}

/// Web page of a release: the search result's uri (relative or absolute), or
/// the generic release URL.
pub fn release_url(uri: Option<&str>, id: u32) -> String {
    match uri.filter(|u| !u.is_empty()) {
        Some(u) if u.starts_with("http") => u.to_string(),
        Some(u) => format!("https://www.discogs.com{u}"),
        None => format!("https://www.discogs.com/release/{id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(instance_id: u32, folder_id: u32) -> CollectionInstance {
        CollectionInstance {
            instance_id,
            folder_id,
        }
    }

    #[test]
    fn keys_are_unique_and_labelled() {
        let keys: Vec<Key> = Command::ALL.iter().map(|c| c.key()).collect();
        for (i, k) in keys.iter().enumerate() {
            assert!(!keys[i + 1..].contains(k), "{k:?} used twice");
        }
        assert_eq!(Command::Add.key(), Key::A);
        assert_eq!(Command::Remove.key_label(), "R");
        assert_eq!(Command::Open.key_label(), "O");
        assert_eq!(Command::Quit.key(), Key::Q);
        assert_eq!(Command::Quit.key_label(), "Q");
        assert!(Command::Remove.needs_selection());
        assert!(!Command::Quit.needs_selection());
        assert!(!Command::ChooseFolder.needs_selection());
    }

    #[test]
    fn add_depends_on_known_ownership() {
        assert_eq!(decide_add(None), AddDecision::NeedsCheck);
        assert_eq!(decide_add(Some(&[])), AddDecision::Add);
        assert_eq!(
            decide_add(Some(&[copy(1, 1)])),
            AddDecision::ConfirmDuplicate(1)
        );
        assert_eq!(
            decide_add(Some(&[copy(1, 1), copy(2, 3)])),
            AddDecision::ConfirmDuplicate(2)
        );
    }

    #[test]
    fn a_just_added_copy_counts_as_owned() {
        assert_eq!(
            decide_add(Some(&[copy(PENDING_INSTANCE, 1)])),
            AddDecision::ConfirmDuplicate(1),
            "pressing A twice quickly must not add twice silently"
        );
    }

    #[test]
    fn remove_picks_the_last_confirmed_copy() {
        assert_eq!(decide_remove(None), RemoveDecision::NeedsCheck);
        assert_eq!(decide_remove(Some(&[])), RemoveDecision::NotOwned);
        assert_eq!(
            decide_remove(Some(&[copy(5, 1), copy(9, 2)])),
            RemoveDecision::Confirm(copy(9, 2))
        );
        assert_eq!(
            decide_remove(Some(&[copy(5, 1), copy(PENDING_INSTANCE, 2)])),
            RemoveDecision::Confirm(copy(5, 1))
        );
        assert_eq!(
            decide_remove(Some(&[copy(PENDING_INSTANCE, 1)])),
            RemoveDecision::NeedsCheck
        );
    }

    #[test]
    fn release_urls() {
        assert_eq!(
            release_url(Some("/release/5-Foo"), 5),
            "https://www.discogs.com/release/5-Foo"
        );
        assert_eq!(
            release_url(Some("https://www.discogs.com/release/5"), 5),
            "https://www.discogs.com/release/5"
        );
        assert_eq!(release_url(None, 5), "https://www.discogs.com/release/5");
        assert_eq!(
            release_url(Some(""), 5),
            "https://www.discogs.com/release/5"
        );
    }
}
