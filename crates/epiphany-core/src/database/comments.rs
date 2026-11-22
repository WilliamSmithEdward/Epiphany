//! Comments: notes pinned to cells, as planners explain a number beside it. They are no
//! part of the cells and change no value; the journal records them, as it does commits.

use serde::{Deserialize, Serialize};

use super::{Database, Error};

/// The longest a comment may be, in characters.
const LONGEST: usize = 10_000;

/// A note pinned to a cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct Comment {
    pub id: u64,
    pub cube: String,
    /// One element name for each dimension of the cube.
    pub at: Vec<String>,
    /// Who wrote it.
    pub author: String,
    /// When it was written, in seconds since 1970.
    pub written: u64,
    pub text: String,
}

/// A change to the comments, as a journal records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommentChange {
    Added(Comment),
    Removed(u64),
}

impl Database {
    /// The comments on cells of a cube, the oldest first.
    pub fn comments(&self, cube: &str) -> Result<Vec<Comment>, Error> {
        let name = &self.model.cubes()[self.cube_id(cube)?].name;
        Ok(self
            .comments
            .iter()
            .filter(|comment| &comment.cube == name)
            .cloned()
            .collect())
    }

    /// Pins a comment to a cell, named by one element name or alias for each dimension of
    /// the cube; the comment keeps the elements' names. Returns the comment as kept.
    pub fn comment(
        &mut self,
        cube: &str,
        at: &[String],
        author: &str,
        written: u64,
        text: &str,
    ) -> Result<Comment, Error> {
        let text = text.trim();
        if text.is_empty() || text.chars().count() > LONGEST {
            return Err(Error::Comment(format!(
                "a comment holds between 1 and {LONGEST} characters"
            )));
        }
        let (id, coordinate) = self.locate(cube, at)?;
        let named = self.named(id, &coordinate, crate::Scalar::Number(0.0));
        let comment = Comment {
            id: self.last_comment + 1,
            cube: named.cube,
            at: named.at,
            author: author.to_owned(),
            written,
            text: text.to_owned(),
        };
        self.record(CommentChange::Added(comment.clone()))?;
        Ok(comment)
    }

    /// Takes a comment away. Returns it.
    pub fn uncomment(&mut self, id: u64) -> Result<Comment, Error> {
        let comment = self
            .comments
            .iter()
            .find(|comment| comment.id == id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("there is no comment {id}")))?;
        self.record(CommentChange::Removed(id))?;
        Ok(comment)
    }

    /// Applies a change recorded before, without recording it again.
    pub fn load_comment(&mut self, change: CommentChange) {
        match change {
            CommentChange::Added(comment) => {
                self.last_comment = self.last_comment.max(comment.id);
                self.comments.push(comment);
            }
            CommentChange::Removed(id) => self.comments.retain(|comment| comment.id != id),
        }
    }

    /// Records a change in the journal, then makes it: one the journal cannot record is
    /// not made.
    fn record(&mut self, change: CommentChange) -> Result<(), Error> {
        if let Some(mut journal) = self.journal() {
            journal.comment(&change)?;
        }
        self.load_comment(change);
        Ok(())
    }
}
