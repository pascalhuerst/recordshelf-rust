//! Stacks: named, ordered lists of records, in the order they were labelled
//! (see db/migrations/0003_stacks.sql).

use anyhow::{Context, Result, bail};

use super::{Library, labels_display};
use crate::catalogue::join_artists;

/// A stack and how many records are on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackInfo {
    pub id: i32,
    pub name: String,
    /// Newly labelled records go on this one.
    pub active: bool,
    pub count: i64,
}

/// A record on a stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackEntry {
    /// 1 for the first record put on the stack.
    pub position: u32,
    pub release_id: u32,
    pub artists: String,
    pub title: String,
    /// "Label (CAT 1)".
    pub labels: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    Added {
        position: u32,
    },
    /// It was on the stack already (a label printed twice): nothing changes.
    Already {
        position: u32,
    },
    /// Only records of our database can be stacked.
    NotInLibrary,
}

impl Library {
    /// All stacks, the active one first, then newest first.
    pub fn list_stacks(&mut self) -> Result<Vec<StackInfo>> {
        Ok(self
            .client
            .query(
                "SELECT s.id, s.name, s.active, count(r.release_id)
                   FROM stacks s LEFT JOIN stack_records r ON r.stack_id = s.id
                  GROUP BY s.id ORDER BY s.active DESC, s.id DESC",
                &[],
            )?
            .iter()
            .map(|row| StackInfo {
                id: row.get(0),
                name: row.get(1),
                active: row.get(2),
                count: row.get(3),
            })
            .collect())
    }

    /// The records of a stack in order.
    pub fn stack_entries(&mut self, stack: i32) -> Result<Vec<StackEntry>> {
        let rows = self.client.query(
            "SELECT row_number() OVER (ORDER BY sr.position), r.id, r.title
               FROM stack_records sr JOIN releases r ON r.id = sr.release_id
              WHERE sr.stack_id = $1 ORDER BY sr.position",
            &[&stack],
        )?;
        let ids: Vec<i32> = rows.iter().map(|r| r.get(1)).collect();
        let mut artists: std::collections::HashMap<i32, Vec<(String, Option<String>)>> =
            Default::default();
        for row in self.client.query(
            "SELECT ra.release_id, a.name, ra.join_text FROM release_artists ra
               JOIN artists a ON a.id = ra.artist_id
              WHERE ra.release_id = ANY($1) ORDER BY ra.release_id, ra.position",
            &[&ids],
        )? {
            artists
                .entry(row.get(0))
                .or_default()
                .push((row.get(1), row.get(2)));
        }
        let mut labels: std::collections::HashMap<i32, Vec<(String, String)>> = Default::default();
        for row in self.client.query(
            "SELECT rl.release_id, l.name, rl.catalog_number FROM release_labels rl
               JOIN labels l ON l.id = rl.label_id
              WHERE rl.release_id = ANY($1) ORDER BY rl.release_id, l.name, rl.catalog_number",
            &[&ids],
        )? {
            labels
                .entry(row.get(0))
                .or_default()
                .push((row.get(1), row.get(2)));
        }
        Ok(rows
            .iter()
            .map(|row| {
                let id: i32 = row.get(1);
                let names = artists.get(&id).map(Vec::as_slice).unwrap_or_default();
                StackEntry {
                    position: row.get::<_, i64>(0) as u32,
                    release_id: id as u32,
                    artists: join_artists(names.iter().map(|(n, j)| (n.as_str(), j.as_deref()))),
                    title: row.get(2),
                    labels: labels_display(labels.get(&id).map(Vec::as_slice).unwrap_or_default()),
                }
            })
            .collect())
    }

    /// Starts a new stack and makes it the active one.
    pub fn create_stack(&mut self, name: &str) -> Result<i32> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a stack needs a name");
        }
        let mut tx = self.client.transaction()?;
        if tx
            .query_opt(
                "SELECT 1 FROM stacks WHERE lower(name) = lower($1)",
                &[&name],
            )?
            .is_some()
        {
            bail!("there is a stack called {name} already");
        }
        tx.execute("UPDATE stacks SET active = false WHERE active", &[])?;
        let id: i32 = tx
            .query_one(
                "INSERT INTO stacks (name, active) VALUES ($1, true) RETURNING id",
                &[&name],
            )?
            .get(0);
        tx.commit()?;
        Ok(id)
    }

    /// Makes a stack the active one, or none (`None`). False if there is no such stack.
    pub fn set_active_stack(&mut self, stack: Option<i32>) -> Result<bool> {
        let mut tx = self.client.transaction()?;
        tx.execute("UPDATE stacks SET active = false WHERE active", &[])?;
        let found = match stack {
            Some(id) => tx.execute("UPDATE stacks SET active = true WHERE id = $1", &[&id])? > 0,
            None => true,
        };
        if found {
            tx.commit()?;
        }
        Ok(found)
    }

    pub fn rename_stack(&mut self, stack: i32, name: &str) -> Result<bool> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a stack needs a name");
        }
        if self
            .client
            .query_opt(
                "SELECT 1 FROM stacks WHERE lower(name) = lower($1) AND id <> $2",
                &[&name, &stack],
            )?
            .is_some()
        {
            bail!("there is a stack called {name} already");
        }
        Ok(self.client.execute(
            "UPDATE stacks SET name = $2 WHERE id = $1",
            &[&stack, &name],
        )? > 0)
    }

    /// Deletes a stack (the records stay in the library).
    pub fn delete_stack(&mut self, stack: i32) -> Result<bool> {
        Ok(self
            .client
            .execute("DELETE FROM stacks WHERE id = $1", &[&stack])?
            > 0)
    }

    /// Puts a record at the end of a stack, unless it is on it already.
    pub fn add_to_stack(&mut self, stack: i32, release: u32) -> Result<AddOutcome> {
        let release = i32::try_from(release).context("release id out of range")?;
        let mut tx = self.client.transaction()?;
        if tx
            .query_opt("SELECT 1 FROM releases WHERE id = $1", &[&release])?
            .is_none()
        {
            return Ok(AddOutcome::NotInLibrary);
        }
        // lock the stack, so that two additions cannot get the same position
        if tx
            .query_opt("SELECT 1 FROM stacks WHERE id = $1 FOR UPDATE", &[&stack])?
            .is_none()
        {
            bail!("the stack does not exist any more");
        }
        if let Some(row) = tx.query_opt(
            "SELECT position FROM stack_records WHERE stack_id = $1 AND release_id = $2",
            &[&stack, &release],
        )? {
            return Ok(AddOutcome::Already {
                position: row.get::<_, i32>(0) as u32,
            });
        }
        let position: i32 = tx
            .query_one(
                "INSERT INTO stack_records (stack_id, position, release_id)
                 SELECT $1, coalesce(max(position), 0) + 1, $2 FROM stack_records
                  WHERE stack_id = $1 RETURNING position",
                &[&stack, &release],
            )?
            .get(0);
        tx.commit()?;
        Ok(AddOutcome::Added {
            position: position as u32,
        })
    }

    /// Takes a record off a stack; the ones behind it move up. False if it was not on it.
    pub fn remove_from_stack(&mut self, stack: i32, release: u32) -> Result<bool> {
        let release = i32::try_from(release).context("release id out of range")?;
        let mut tx = self.client.transaction()?;
        let removed = tx.execute(
            "DELETE FROM stack_records WHERE stack_id = $1 AND release_id = $2",
            &[&stack, &release],
        )? > 0;
        renumber(&mut tx, stack)?;
        tx.commit()?;
        Ok(removed)
    }

    /// Moves a record to `position` (1 = first; beyond the end = last).
    /// False if it is not on the stack.
    pub fn move_in_stack(&mut self, stack: i32, release: u32, position: u32) -> Result<bool> {
        let release = i32::try_from(release).context("release id out of range")?;
        let mut tx = self.client.transaction()?;
        let mut order: Vec<i32> = tx
            .query(
                "SELECT release_id FROM stack_records WHERE stack_id = $1 ORDER BY position",
                &[&stack],
            )?
            .iter()
            .map(|r| r.get(0))
            .collect();
        let Some(from) = order.iter().position(|id| *id == release) else {
            return Ok(false);
        };
        order.remove(from);
        let to = (position.max(1) as usize - 1).min(order.len());
        order.insert(to, release);
        for (i, id) in order.iter().enumerate() {
            tx.execute(
                "UPDATE stack_records SET position = $3 WHERE stack_id = $1 AND release_id = $2",
                &[&stack, id, &(i as i32 + 1)],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }
}

/// Numbers the records of a stack 1, 2, 3 ... in their current order (the
/// primary key is only checked at the end of the transaction).
fn renumber(tx: &mut postgres::Transaction<'_>, stack: i32) -> Result<()> {
    tx.execute(
        "UPDATE stack_records s SET position = n.rn
           FROM (SELECT release_id, row_number() OVER (ORDER BY position)::int AS rn
                   FROM stack_records WHERE stack_id = $1) n
          WHERE s.stack_id = $1 AND s.release_id = n.release_id",
        &[&stack],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use crate::catalogue::CatalogueRelease;
    use crate::discogs::Release;

    fn save(library: &mut Library, id: u32, artist: &str, title: &str) {
        let json = format!(
            r#"{{"id": {id}, "title": "{title}", "artists": [{{"id": {id}, "name": "{artist}"}}],
                "labels": [{{"id": 5, "name": "Lbl", "catno": "L{id}"}}]}}"#
        );
        let release: Release = serde_json::from_str(&json).unwrap();
        library
            .save_release(&CatalogueRelease::from_release(id, &release, 1).unwrap())
            .unwrap();
    }

    fn order(library: &mut Library, stack: i32) -> Vec<u32> {
        library
            .stack_entries(stack)
            .unwrap()
            .iter()
            .map(|e| e.release_id)
            .collect()
    }

    fn library_with_records(name: &str) -> Option<(Library, String)> {
        let (mut library, url) = library(name)?;
        for id in 1..=4 {
            save(
                &mut library,
                id,
                &format!("Artist {id}"),
                &format!("Title {id}"),
            );
        }
        Some((library, url))
    }

    #[test]
    fn records_are_stacked_in_the_order_they_are_added() {
        let Some((mut library, _)) = library_with_records("t_stack_order") else {
            return;
        };
        let stack = library.create_stack("  Box A ").unwrap();
        for id in [3, 1, 4] {
            library.add_to_stack(stack, id).unwrap();
        }
        assert_eq!(order(&mut library, stack), [3, 1, 4]);
        let entries = library.stack_entries(stack).unwrap();
        assert_eq!(entries[0].position, 1);
        assert_eq!(entries[2].position, 3);
        assert_eq!(entries[0].artists, "Artist 3");
        assert_eq!(entries[0].title, "Title 3");
        assert_eq!(entries[0].labels, "Lbl (L3)");
        let stacks = library.list_stacks().unwrap();
        assert_eq!(stacks.len(), 1);
        assert_eq!((stacks[0].name.as_str(), stacks[0].count), ("Box A", 3));
    }

    #[test]
    fn a_record_is_on_a_stack_only_once_but_may_be_on_several() {
        let Some((mut library, _)) = library_with_records("t_stack_twice") else {
            return;
        };
        let a = library.create_stack("A").unwrap();
        let b = library.create_stack("B").unwrap();
        assert_eq!(
            library.add_to_stack(a, 1).unwrap(),
            AddOutcome::Added { position: 1 }
        );
        library.add_to_stack(a, 2).unwrap();
        assert_eq!(
            library.add_to_stack(a, 1).unwrap(),
            AddOutcome::Already { position: 1 },
            "a label printed twice does not stack twice"
        );
        assert_eq!(
            library.add_to_stack(b, 1).unwrap(),
            AddOutcome::Added { position: 1 }
        );
        assert_eq!(order(&mut library, a), [1, 2]);
        assert_eq!(order(&mut library, b), [1]);
    }

    #[test]
    fn only_records_of_the_library_can_be_stacked() {
        let Some((mut library, _)) = library_with_records("t_stack_unknown") else {
            return;
        };
        let stack = library.create_stack("A").unwrap();
        assert_eq!(
            library.add_to_stack(stack, 999).unwrap(),
            AddOutcome::NotInLibrary
        );
        assert!(library.add_to_stack(stack + 100, 1).is_err());
        assert!(order(&mut library, stack).is_empty());
    }

    #[test]
    fn the_newest_stack_is_the_active_one() {
        let Some((mut library, _)) = library_with_records("t_stack_active") else {
            return;
        };
        let a = library.create_stack("A").unwrap();
        let b = library.create_stack("B").unwrap();
        let stacks = library.list_stacks().unwrap();
        assert_eq!(stacks[0].id, b);
        assert!(stacks[0].active && !stacks[1].active);
        assert!(library.set_active_stack(Some(a)).unwrap());
        assert_eq!(library.list_stacks().unwrap()[0].id, a);
        assert!(library.set_active_stack(None).unwrap());
        assert!(library.list_stacks().unwrap().iter().all(|s| !s.active));
        assert!(!library.set_active_stack(Some(999)).unwrap());
        assert!(
            library.list_stacks().unwrap().iter().all(|s| !s.active),
            "an unknown stack changes nothing"
        );
    }

    #[test]
    fn names_are_unique_and_not_blank() {
        let Some((mut library, _)) = library_with_records("t_stack_names") else {
            return;
        };
        let a = library.create_stack("Box A").unwrap();
        let err = library.create_stack("box a").unwrap_err().to_string();
        assert!(err.contains("already"), "{err}");
        assert!(library.create_stack("   ").is_err());
        let b = library.create_stack("Box B").unwrap();
        assert!(library.rename_stack(b, "BOX A").is_err());
        assert!(
            library.rename_stack(a, "Box a").unwrap(),
            "its own name is fine"
        );
        assert!(library.rename_stack(b, "Techno").unwrap());
        assert!(library.rename_stack(b, " ").is_err());
        assert!(!library.rename_stack(999, "X").unwrap());
        assert!(
            library
                .list_stacks()
                .unwrap()
                .iter()
                .any(|s| s.name == "Techno")
        );
    }

    #[test]
    fn taking_a_record_out_closes_the_gap() {
        let Some((mut library, url)) = library_with_records("t_stack_remove") else {
            return;
        };
        let stack = library.create_stack("A").unwrap();
        for id in 1..=4 {
            library.add_to_stack(stack, id).unwrap();
        }
        assert!(library.remove_from_stack(stack, 2).unwrap());
        assert!(!library.remove_from_stack(stack, 2).unwrap());
        assert_eq!(order(&mut library, stack), [1, 3, 4]);
        let positions: Vec<i32> = raw(&url)
            .query("SELECT position FROM stack_records ORDER BY position", &[])
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(positions, [1, 2, 3]);
        // the next one goes to the end
        assert_eq!(
            library.add_to_stack(stack, 2).unwrap(),
            AddOutcome::Added { position: 4 }
        );
    }

    #[test]
    fn records_can_be_moved_within_a_stack() {
        let Some((mut library, _)) = library_with_records("t_stack_move") else {
            return;
        };
        let stack = library.create_stack("A").unwrap();
        for id in 1..=4 {
            library.add_to_stack(stack, id).unwrap();
        }
        assert!(library.move_in_stack(stack, 4, 1).unwrap());
        assert_eq!(order(&mut library, stack), [4, 1, 2, 3]);
        assert!(library.move_in_stack(stack, 4, 3).unwrap());
        assert_eq!(order(&mut library, stack), [1, 2, 4, 3]);
        assert!(library.move_in_stack(stack, 1, 99).unwrap());
        assert_eq!(order(&mut library, stack), [2, 4, 3, 1]);
        assert!(
            library.move_in_stack(stack, 2, 0).unwrap(),
            "0 counts as first"
        );
        assert_eq!(order(&mut library, stack), [2, 4, 3, 1]);
        assert!(!library.move_in_stack(stack, 999, 1).unwrap());
    }

    #[test]
    fn deleting_a_stack_keeps_the_records_and_deleting_a_record_leaves_the_stack() {
        let Some((mut library, url)) = library_with_records("t_stack_delete") else {
            return;
        };
        let a = library.create_stack("A").unwrap();
        let b = library.create_stack("B").unwrap();
        for id in 1..=3 {
            library.add_to_stack(a, id).unwrap();
            library.add_to_stack(b, id).unwrap();
        }
        // a record deleted from the library disappears from the stacks; the order stays
        assert!(library.delete_release(2).unwrap());
        assert_eq!(order(&mut library, a), [1, 3]);
        let entries = library.stack_entries(a).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.position).collect::<Vec<_>>(),
            [1, 2],
            "shown without a gap"
        );
        assert!(library.delete_stack(a).unwrap());
        assert!(!library.delete_stack(a).unwrap());
        assert_eq!(order(&mut library, b), [1, 3]);
        let releases: i64 = raw(&url)
            .query_one("SELECT count(*) FROM releases", &[])
            .unwrap()
            .get(0);
        assert_eq!(releases, 3);
    }
}
