use fm::FileId;
use noirc_span::Span;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    hash::{Hash, Hasher},
};

pub type Position = u32;

#[derive(Eq, Debug, Clone)]
pub struct Located<T> {
    pub contents: T,
    location: Location,
}

/// This is important for tests. Two Located objects are equal if their content is equal
/// They may not have the same location. Use `.location()` to test for Location being equal specifically
impl<T: PartialEq> PartialEq<Located<T>> for Located<T> {
    fn eq(&self, other: &Located<T>) -> bool {
        self.contents == other.contents
    }
}

impl<T: PartialOrd> PartialOrd<Located<T>> for Located<T> {
    fn partial_cmp(&self, other: &Located<T>) -> Option<Ordering> {
        self.contents.partial_cmp(&other.contents)
    }
}

impl<T: Ord> Ord for Located<T> {
    fn cmp(&self, other: &Located<T>) -> Ordering {
        self.contents.cmp(&other.contents)
    }
}

impl<T: Default> Default for Located<T> {
    fn default() -> Self {
        Self { contents: Default::default(), location: Location::dummy() }
    }
}

/// Hash-based data structures (`HashMap`, `HashSet`) rely on the inverse of Hash
/// being injective, i.e. x.eq(y) => hash(x, H) == hash(y, H), we hence align
/// this with the above
impl<T: Hash> Hash for Located<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.contents.hash(state);
    }
}

impl<T> Located<T> {
    pub fn from(location: Location, contents: T) -> Located<T> {
        Located { location, contents }
    }

    pub fn span(&self) -> Span {
        self.location.span
    }

    pub fn location(&self) -> Location {
        self.location
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Location {
    pub span: Span,
    pub file: FileId,
}

// nemi patch: the same hash as `#[derive(Hash)]` (fields in declaration order), except while
// `LocationShifts::hash_translated` runs on this thread.
impl Hash for Location {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let translated = HASH_SHIFTS.with(|shifts| match &*shifts.borrow() {
            None => Some(*self),
            Some(shifts) => shifts.translate(*self),
        });
        match translated {
            Some(Location { span, file }) => {
                span.hash(state);
                file.hash(state);
            }
            // no real location hashes like this: its file id and span end are out of range
            None => {
                state.write_u32(u32::MAX);
                state.write_u32(0);
                state.write_usize(usize::MAX);
            }
        }
    }
}

/// How the source files changed between two compilations, in a form that maps locations of the new sources to the
/// same code in the old ones (nemi patch).
#[derive(Clone, Debug, Default)]
pub struct LocationShifts {
    /// For each file id of the new sources: the file id and edit in the old ones. Files not listed have no
    /// translation.
    pub files: std::collections::HashMap<FileId, (FileId, FileEdit)>,
}

/// One contiguous edit of a file: `prefix` bytes are unchanged at the start, then `old_changed` bytes were replaced by
/// `new_changed` bytes, and the rest is unchanged. An unchanged file has no changed bytes.
#[derive(Clone, Copy, Debug)]
pub struct FileEdit {
    pub prefix: u32,
    pub old_changed: u32,
    pub new_changed: u32,
}

impl FileEdit {
    /// The single contiguous edit that turns `old` into `new`: the longest common prefix and suffix.
    pub fn between(old: &[u8], new: &[u8]) -> Self {
        let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        let max_suffix = old.len().min(new.len()) - prefix;
        let suffix =
            old.iter().rev().zip(new.iter().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
        Self {
            prefix: prefix as u32,
            old_changed: (old.len() - prefix - suffix) as u32,
            new_changed: (new.len() - prefix - suffix) as u32,
        }
    }

    /// Maps a span through the edit, from the side whose changed region has length `from` to the other (`to`): spans
    /// in the unchanged prefix stay, spans in the unchanged suffix move, spans touching the changed region have no
    /// image. An empty span at the boundary of the changed region also has none, as it is ambiguous.
    fn map(&self, span: Span, from: u32, to: u32) -> Option<Span> {
        let (start, end) = (span.start(), span.end());
        let changed_end = self.prefix + from;
        if from == 0 && to == 0 {
            Some(span)
        } else if end < self.prefix || (end == self.prefix && start < end) {
            Some(span)
        } else if start > changed_end || (start == changed_end && start < end) {
            Some(Span::from(start - from + to..end - from + to))
        } else {
            None
        }
    }
}

impl LocationShifts {
    /// The location of the same code in the old sources, if it is in unchanged code.
    pub fn translate(&self, location: Location) -> Option<Location> {
        let (old_file, edit) = self.files.get(&location.file)?;
        let span = edit.map(location.span, edit.new_changed, edit.old_changed)?;
        Some(Location::new(span, *old_file))
    }

    /// The inverse of `translate`: the location in the new sources of code at `location` in the old ones.
    /// Only for shifts whose files keep their ids (`files` maps each id to itself).
    pub fn untranslate(&self, location: Location) -> Option<Location> {
        let (old_file, edit) = self.files.get(&location.file)?;
        debug_assert_eq!(*old_file, location.file, "untranslate needs files to keep their ids");
        let span = edit.map(location.span, edit.old_changed, edit.new_changed)?;
        Some(Location::new(span, location.file))
    }

    /// Runs `f` with every `Location` hashed on this thread translated to the old sources first, so that a program
    /// hashes as if its locations were those of the old sources. A location with no translation hashes to a value no
    /// real location hashes to.
    pub fn hash_translated<R>(self, f: impl FnOnce() -> R) -> (R, Self) {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                HASH_SHIFTS.with(|shifts| shifts.borrow_mut().take());
            }
        }
        HASH_SHIFTS.with(|shifts| *shifts.borrow_mut() = Some(self));
        let reset = Reset;
        let result = f();
        let shifts = HASH_SHIFTS.with(|shifts| shifts.borrow_mut().take()).expect("set above");
        drop(reset);
        (result, shifts)
    }
}

thread_local! {
    static HASH_SHIFTS: std::cell::RefCell<Option<LocationShifts>> = const { std::cell::RefCell::new(None) };
}

impl Location {
    pub fn new(span: Span, file: FileId) -> Self {
        Self { span, file }
    }

    pub const fn dummy() -> Self {
        Self { span: Span::initial(), file: FileId::dummy() }
    }

    pub fn contains(&self, other: &Location) -> bool {
        self.file == other.file && self.span.contains(&other.span)
    }

    #[must_use]
    pub fn merge(self, other: Location) -> Location {
        if self.file == other.file {
            Location::new(self.span.merge(other.span), self.file)
        } else {
            self
        }
    }

    pub fn is_dummy(&self) -> bool {
        *self == Location::dummy()
    }
}

impl Ord for Location {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.file, self.span).cmp(&(other.file, other.span))
    }
}

impl PartialOrd for Location {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Debug for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}:{}", self.file.as_usize(), self.span.start(), self.span.end())
    }
}
