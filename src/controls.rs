//! Single-key commands for working through releases, and the decisions behind
//! them. Kept free of UI code so the rules (e.g. when to ask before adding a
//! duplicate) can be tested.

use eframe::egui::Key;

use crate::discogs::CollectionInstance;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Add,
    Remove,
    Move,
    Open,
    Print,
    ChooseFolder,
    Quit,
}

impl Command {
    pub const ALL: [Command; 7] = [
        Command::Add,
        Command::Remove,
        Command::Move,
        Command::Open,
        Command::Print,
        Command::ChooseFolder,
        Command::Quit,
    ];

    pub fn key(self) -> Key {
        match self {
            Command::Add => Key::A,
            Command::Remove => Key::R,
            Command::Move => Key::M,
            Command::Open => Key::O,
            Command::Print => Key::P,
            Command::ChooseFolder => Key::F,
            Command::Quit => Key::Q,
        }
    }

    /// Whether the command acts on the selected release.
    pub fn needs_selection(self) -> bool {
        !matches!(self, Command::ChooseFolder | Command::Quit)
    }

    /// Whether the command is only offered for releases we own (a label can
    /// only be printed for a record that is on the shelf).
    pub fn needs_ownership(self) -> bool {
        matches!(self, Command::Print)
    }

    /// Key as shown to the user.
    pub fn key_label(self) -> &'static str {
        match self {
            Command::Add => "A",
            Command::Remove => "R",
            Command::Move => "M",
            Command::Open => "O",
            Command::Print => "P",
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

#[derive(Debug, PartialEq, Eq)]
pub enum MoveDecision {
    /// The confirmed copies that could be moved.
    Pick(Vec<CollectionInstance>),
    NotOwned,
    /// Collection status unknown or still changing; try again in a moment.
    NeedsCheck,
}

pub fn decide_move(owned: Option<&[CollectionInstance]>) -> MoveDecision {
    match owned {
        None => MoveDecision::NeedsCheck,
        Some([]) => MoveDecision::NotOwned,
        Some(copies) => {
            let confirmed: Vec<_> = copies
                .iter()
                .filter(|c| c.instance_id != PENDING_INSTANCE)
                .cloned()
                .collect();
            if confirmed.is_empty() {
                MoveDecision::NeedsCheck
            } else {
                MoveDecision::Pick(confirmed)
            }
        }
    }
}

/// The copies that actually have to change folder (those already in `to_folder`
/// stay where they are).
pub fn plan_move(copies: &[CollectionInstance], to_folder: u32) -> Vec<CollectionInstance> {
    copies
        .iter()
        .filter(|c| c.folder_id != to_folder)
        .cloned()
        .collect()
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
        assert_eq!(Command::Move.key(), Key::M);
        assert_eq!(Command::Move.key_label(), "M");
        assert!(Command::Move.needs_selection());
        assert_eq!(Command::Print.key(), Key::P);
        assert_eq!(Command::Print.key_label(), "P");
        assert!(Command::Print.needs_selection() && Command::Print.needs_ownership());
        assert!(!Command::Add.needs_ownership() && !Command::Open.needs_ownership());
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

    #[test]
    fn move_offers_only_confirmed_copies() {
        assert_eq!(decide_move(None), MoveDecision::NeedsCheck);
        assert_eq!(decide_move(Some(&[])), MoveDecision::NotOwned);
        assert_eq!(
            decide_move(Some(&[copy(5, 1), copy(PENDING_INSTANCE, 2)])),
            MoveDecision::Pick(vec![copy(5, 1)])
        );
        assert_eq!(
            decide_move(Some(&[copy(PENDING_INSTANCE, 2)])),
            MoveDecision::NeedsCheck
        );
    }

    #[test]
    fn plan_move_skips_copies_already_in_the_target() {
        let copies = [copy(1, 4), copy(2, 7), copy(3, 4)];
        assert_eq!(plan_move(&copies, 4), vec![copy(2, 7)]);
        assert_eq!(plan_move(&copies, 9), copies.to_vec());
        assert!(plan_move(&[copy(1, 4)], 4).is_empty());
    }
}
