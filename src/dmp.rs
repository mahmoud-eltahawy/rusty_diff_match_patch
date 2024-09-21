/*Functions for diff, match and patch.

Computes the difference between two texts to create a patch.
Applies the patch onto another text, allowing for errors.
*/

use regex::Regex;
use std::cmp::{max, min};
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Display};
use std::iter::FromIterator;
use std::result::Result;
use std::time::Instant;

use super::percent_encoding::percent_decode_u16;

use url::percent_encoding::{percent_decode, utf8_percent_encode, USERINFO_ENCODE_SET};

pub enum LengthUnit {
    UnicodeScalar,
    UTF16,
}

pub struct Dmp {
    // Number of seconds to map a diff before giving up (None for infinity).
    pub diff_timeout: Option<f32>,
    // Cost of an empty edit operation() in terms of edit characters.
    pub edit_cost: usize,
    /*How far to search for a match (0 = exact location, 1000+ = broad match).
    A match this many characters away from the expected location will add
    1.0 to the score (0.0 is a perfect match).*/
    pub match_distance: usize,
    // Chunk size for context length.
    pub patch_margin: usize,
    /*The number of bits in an int.
    Python has no maximum, thus to disable patch splitting set to 0.
    However to avoid long patches in certain pathological cases, use 32.
    Multiple short patches (using native ints) are much faster than long ones.*/
    pub match_maxbits: usize,
    // At what point is no match declared (0.0 = perfection, 1.0 = very loose).
    pub match_threshold: f32,
    /*When deleting a large block of text (over ~64 characters), how close do
    the contents have to be to match the expected contents. (0.0 = perfection,
    1.0 = very loose).  Note that Match_Threshold controls how closely the
    end points of a delete need to match.*/
    pub patch_delete_threshold: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Diff {
    Add(String),
    Keep(String),
    Delete(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Patch {
    //patch object
    pub diffs: Vec<Diff>,
    pub start1: usize,
    pub start2: usize,
    pub length1: usize,
    pub length2: usize,
}

impl Diff {
    pub fn text(&self) -> &String {
        match self {
            Diff::Add(text) => text,
            Diff::Keep(text) => text,
            Diff::Delete(text) => text,
        }
    }
    pub fn with_text(&self, text: String) -> Self {
        match self {
            Diff::Add(_) => Self::Add(text),
            Diff::Keep(_) => Self::Keep(text),
            Diff::Delete(_) => Self::Delete(text),
        }
    }
    pub fn set_text(&mut self, text: String) {
        *self = match self {
            Diff::Add(_) => Diff::Add(text),
            Diff::Keep(_) => Diff::Keep(text),
            Diff::Delete(_) => Diff::Delete(text),
        };
    }
    pub fn append_text(&mut self, text: &String) {
        *self = match self {
            Diff::Add(t) => Diff::Add(t.clone() + text),
            Diff::Keep(t) => Diff::Keep(t.clone() + text),
            Diff::Delete(t) => Diff::Delete(t.clone() + text),
        };
    }
}

impl Patch {
    pub fn new(
        diffs: Vec<Diff>,
        start1: usize,
        start2: usize,
        length1: usize,
        length2: usize,
    ) -> Patch {
        Patch {
            diffs,
            start1,
            start2,
            length1,
            length2,
        }
    }
}

/// return minimum element.
fn min1(x: f32, y: f32) -> f32 {
    if x > y {
        return y;
    }
    x
}

/// it will return the first index of a character after a index or return -1 if not found.
fn find_char(cha: char, text: &[char], start: usize) -> i32 {
    text.iter()
        .enumerate()
        .skip(start)
        .find(|(_, text_item)| **text_item == cha)
        .map(|(i, _)| i as i32)
        .unwrap_or(-1)
}

trait StringView {
    fn len(&self) -> usize;
    fn slice(&self, range: std::ops::Range<usize>) -> Result<String, std::string::FromUtf16Error>;
}

struct StringScalarView {
    text: Vec<char>,
}

impl StringScalarView {
    pub fn new(text: &str) -> StringScalarView {
        StringScalarView {
            text: text.chars().collect(),
        }
    }
}

impl StringView for StringScalarView {
    fn len(&self) -> usize {
        self.text.len()
    }

    fn slice(&self, range: std::ops::Range<usize>) -> Result<String, std::string::FromUtf16Error> {
        Ok((&self.text)[range].iter().collect())
    }
}

struct StringUTF16View {
    text: Vec<u16>,
}

impl StringUTF16View {
    pub fn new(text: &str) -> StringUTF16View {
        StringUTF16View {
            text: text.encode_utf16().collect(),
        }
    }
}

impl StringView for StringUTF16View {
    fn len(&self) -> usize {
        self.text.len()
    }

    fn slice(&self, range: std::ops::Range<usize>) -> Result<String, std::string::FromUtf16Error> {
        String::from_utf16(&self.text[range])
    }
}

impl Default for Dmp {
    fn default() -> Self {
        Self {
            diff_timeout: None,
            patch_delete_threshold: 0.5,
            edit_cost: 0,
            match_distance: 1000,
            patch_margin: 4,
            match_maxbits: 32,
            match_threshold: 0.5,
        }
    }
}

impl Dmp {
    /// Find the differences between two chars.  Simplifies the problem by
    /// stripping any common prefix or suffix off the texts before diffing.
    ///
    /// Args:
    ///     text1: Old chars to be diffed.
    ///     text2: New chars to be diffed.
    ///     checklines: Optional speedup flag. If present and false, then don't run
    ///         a line-level diff first to identify the changed areas.
    ///         Defaults to true, which does a faster, slightly less optimal diff.
    /// Returns:
    ///     Vector of diffs as changes.
    pub fn diff_main(&self, text1: &str, text2: &str, checklines: bool) -> Vec<Diff> {
        self.diff_main_internal(text1, text2, checklines, Instant::now())
    }

    fn diff_main_internal(
        &self,
        text1: &str,
        text2: &str,
        checklines: bool,
        start_time: Instant,
    ) -> Vec<Diff> {
        match (text1.is_empty(), text2.is_empty()) {
            (true, true) => {
                return vec![];
            }
            (true, false) => {
                return vec![Diff::Add(text2.to_string())];
            }
            (false, true) => {
                return vec![Diff::Delete(text1.to_string())];
            }
            (false, false) => {
                if text1 == text2 {
                    return vec![Diff::Keep(text1.to_string())];
                }
            }
        };

        let text1: Vec<char> = text1.chars().collect();
        let text2: Vec<char> = text2.chars().collect();

        let commonlength = self.diff_common_prefix(&text1, &text2);
        let (commonprefix, text1) = text1.split_at(commonlength);
        let (_, text2) = text2.split_at(commonlength);

        let commonlength = self.diff_common_suffix(text1, text2);
        let (text1, commonsuffix) = text1.split_at(text1.len() - commonlength);
        let (text2, _) = text2.split_at(text2.len() - commonlength);

        let mut diffs: Vec<Diff> = Vec::new();

        //Restore the prefix
        if !commonprefix.is_empty() {
            diffs.push(Diff::Keep(commonprefix.iter().collect()));
        }

        // Compute the diff on the middle block.
        let middle_diffs = self.diff_compute(text1, text2, checklines, start_time);
        diffs.extend(middle_diffs);

        // Restore the suffix
        if !commonsuffix.is_empty() {
            diffs.push(Diff::Keep(commonsuffix.iter().collect()));
        }
        self.diff_cleanup_merge(&mut diffs);
        diffs
    }

    /// Find the differences between two texts.  Assumes that the texts do not
    /// have any common prefix or suffix.
    ///
    /// Args:
    ///     text1: Old chars to be diffed.
    ///     text2: New chars to be diffed.
    ///     checklines: Speedup flag.  If false, then don't run a line-level diff
    ///     first to identify the changed areas.
    ///     If true, then run a faster, slightly less optimal diff.
    ///
    /// Returns:
    ///     Vector of diffs as changes.
    fn diff_compute(
        &self,
        text1: &[char],
        text2: &[char],
        checklines: bool,
        start_time: Instant,
    ) -> Vec<Diff> {
        let mut diffs: Vec<Diff> = Vec::new();
        if text1.is_empty() {
            // Just add some text (speedup).
            diffs.push(Diff::Add(text2.iter().collect()));
            return diffs;
        } else if text2.is_empty() {
            // Just delete some text (speedup).
            diffs.push(Diff::Delete(text1.iter().collect()));
            return diffs;
        }
        {
            let len1 = text1.len();
            let len2 = text2.len();
            let (longtext, shorttext) = if len1 >= len2 {
                (text1, text2)
            } else {
                (text2, text1)
            };
            let i = self.kmp(longtext, shorttext, 0);
            if let Some(i) = i {
                // Shorter text is inside the longer text (speedup).
                if len1 > len2 {
                    if i != 0 {
                        diffs.push(Diff::Delete((text1[0..i]).iter().collect()));
                    }
                    diffs.push(Diff::Keep(text2.iter().collect()));
                    if i + text2.len() != text1.len() {
                        diffs.push(Diff::Delete(text1[(i + text2.len())..].iter().collect()));
                    }
                } else {
                    if i != 0 {
                        diffs.push(Diff::Add((text2[0..i]).iter().collect()));
                    }
                    diffs.push(Diff::Keep(text1.iter().collect()));
                    if i + text1.len() != text2.len() {
                        diffs.push(Diff::Add(text2[(i + text1.len())..].iter().collect()));
                    }
                }
                return diffs;
            }
            if shorttext.len() == 1 {
                // Single character string.
                // After the previous speedup, the character can't be an equality.
                diffs.push(Diff::Delete(text1.iter().collect()));
                diffs.push(Diff::Add(text2.iter().collect()));
                return diffs;
            }
        }
        // Check to see if the problem can be split in two.
        if let Some([text1_a, text1_b, text2_a, text2_b, mid_common]) =
            self.diff_half_match(text1, text2)
        {
            // A half-match was found, sort out the return data.
            // Send both pairs off for separate processing.
            let mut diffs_a =
                self.diff_main_internal(text1_a.as_str(), text2_a.as_str(), checklines, start_time);
            let diffs_b =
                self.diff_main_internal(text1_b.as_str(), text2_b.as_str(), checklines, start_time);
            diffs_a.push(Diff::Keep(mid_common));
            // Merge the result.
            for x in diffs_b {
                diffs_a.push(x);
            }
            return diffs_a;
        }

        if checklines && text1.len() > 100 && text2.len() > 100 {
            return self.diff_linemode_internal(text1, text2, start_time);
        }
        self.diff_bisect_internal(text1, text2, start_time)
    }

    /// Find the first index after a specific index in text1 where patern is present.
    ///
    /// Args:
    ///     text1: Parent chars.
    ///     text2: Patern chars.
    ///     ind: index after which we have to find the patern.
    ///
    /// Returns:
    ///     the first index where patern is found or -1 if not found.
    fn kmp(&self, text1: &[char], text2: &[char], ind: usize) -> Option<usize> {
        if text2.is_empty() {
            return Some(ind);
        }
        if text1.is_empty() {
            return None;
        }
        let len1 = text1.len();
        let len2 = text2.len();
        let mut patern: Vec<usize> = Vec::new();
        patern.push(0);
        let mut len = 0;
        let mut i = 1;

        // Preprocess the pattern
        while i < len2 {
            if text2[i] == text2[len] {
                len += 1;
                patern.push(len);
                i += 1;
            } else if len == 0 {
                patern.push(0);
                i += 1;
            } else {
                len = patern[len - 1];
            }
        }
        i = ind;
        len = 0;
        while i < len1 {
            if text1[i] == text2[len] {
                len += 1;
                i += 1;
                if len == len2 {
                    return Some(i - len);
                }
            } else if len == 0 {
                i += 1;
            } else {
                len = patern[len - 1];
            }
        }
        None
    }

    /// Find the last index before a specific index in text1 where patern is present.
    ///
    /// Args:
    ///     text1: Parent chars.
    ///     text2: Patern chars.
    ///     ind: index just before we have to find the patern.
    ///
    /// Returns:
    ///     the last index where patern is found or -1 if not found.
    fn rkmp(&self, text1: &[char], text2: &[char], end: usize) -> Option<usize> {
        if text2.is_empty() {
            return Some(end);
        }
        if text1.is_empty() {
            return None;
        }
        let len2 = text2.len();
        let mut patern: Vec<usize> = Vec::new();
        patern.push(0);
        let mut len = 0;
        let mut i = 1;

        // Preprocess the pattern
        while i < len2 {
            if text2[i] == text2[len] {
                len += 1;
                patern.push(len);
                i += 1;
            } else if len == 0 {
                patern.push(0);
                i += 1;
            } else {
                len = patern[len - 1];
            }
        }
        let mut i = 0;
        let mut len = 0;
        let mut ans = None::<usize>;
        while i <= end {
            if text1[i] == text2[len] {
                len += 1;
                i += 1;
                if len == len2 {
                    ans = Some(i - len);
                    len = patern[len - 1];
                }
            } else if len == 0 {
                i += 1;
            } else {
                len = patern[len - 1];
            }
        }
        ans
    }

    /// Do a quick line-level diff on both chars, then rediff the parts for
    /// greater accuracy.
    /// This speedup can produce non-minimal diffs.
    ///
    /// Args:
    ///     text1: Old chars to be diffed.
    ///     text2: New chars to be diffed.
    ///
    /// Returns:
    ///     Vector of diffs as changes.
    pub fn diff_linemode(&self, text1: &[char], text2: &[char]) -> Vec<Diff> {
        self.diff_linemode_internal(text1, text2, Instant::now())
    }

    fn diff_linemode_internal(
        &self,
        text1: &[char],
        text2: &[char],
        start_time: Instant,
    ) -> Vec<Diff> {
        // Scan the text on a line-by-line basis first.
        let (text3, text4, linearray) = self.diff_lines_tochars(text1, text2);

        let dmp = Dmp::default();
        let mut diffs: Vec<Diff> =
            dmp.diff_main_internal(text3.as_str(), text4.as_str(), false, start_time);

        // Convert the diff back to original text.
        self.diff_chars_tolines(&mut diffs, &linearray);
        // Eliminate freak matches (e.g. blank lines)
        self.diff_cleanup_semantic(&mut diffs);

        // Rediff any replacement blocks, this time character-by-character.
        // Add a dummy entry at the end.
        diffs.push(Diff::Keep("".to_string()));
        let mut count_delete = 0;
        let mut count_insert = 0;
        let mut text_delete: String = "".to_string();
        let mut text_insert: String = "".to_string();
        let mut pointer = 0;
        let mut temp: Vec<Diff> = vec![];
        while pointer < diffs.len() {
            match &diffs[pointer] {
                Diff::Add(txt) => {
                    count_insert += 1;
                    text_insert += txt;
                }
                Diff::Delete(txt) => {
                    count_delete += 1;
                    text_delete += txt;
                }
                Diff::Keep(txt) => {
                    // Upon reaching an equality, check for prior redundancies.
                    if count_delete >= 1 && count_insert >= 1 {
                        // Delete the offending records and add the merged ones.
                        let sub_diff = self.diff_main_internal(
                            text_delete.as_str(),
                            text_insert.as_str(),
                            false,
                            start_time,
                        );
                        for z in sub_diff {
                            temp.push(z);
                        }
                        temp.push(diffs[pointer].with_text(txt.clone()));
                    } else {
                        if !text_delete.is_empty() {
                            temp.push(Diff::Delete(text_delete));
                        }
                        if !text_insert.is_empty() {
                            temp.push(Diff::Add(text_insert));
                        }
                        temp.push(diffs[pointer].with_text(txt.clone()));
                    }
                    count_delete = 0;
                    count_insert = 0;
                    text_delete = "".to_string();
                    text_insert = "".to_string();
                }
            }
            pointer += 1;
        }
        temp.pop(); //Remove the dummy entry at the end.
        temp
    }

    /// Find the 'middle snake' of a diff, split the problem in two
    /// and return the recursively constructed diff.
    /// See Myers 1986 paper: An O(ND) Difference Algorithm and Its Variations.
    ///
    /// Args:
    ///     text1: Old chars to be diffed.
    ///     text2: New chars to be diffed.
    ///
    /// Returns:
    ///         Vector of diffs as changes.
    pub fn diff_bisect(&self, char1: &[char], char2: &[char]) -> Vec<Diff> {
        self.diff_bisect_internal(char1, char2, Instant::now())
    }

    fn diff_bisect_internal(
        &self,
        char1: &[char],
        char2: &[char],
        start_time: Instant,
    ) -> Vec<Diff> {
        let text1_length = char1.len() as i32;
        let text2_length = char2.len() as i32;
        let max_d: i32 = (text1_length + text2_length + 1) / 2;
        let v_offset: i32 = max_d;
        let v_length: i32 = 2 * max_d;
        let mut v1: Vec<i32> = vec![-1; v_length as usize];
        let mut v2: Vec<i32> = vec![-1; v_length as usize];
        v1[v_offset as usize + 1] = 0;
        v2[v_offset as usize + 1] = 0;
        let delta: i32 = text1_length - text2_length;
        // If the total number of characters is odd, then the front path will
        // collide with the reverse path.
        let front: i32 = (delta % 2 != 0) as i32;
        // Offsets for start and end of k loop.
        // Prevents mapping of space beyond the grid.
        let mut k1start: i32 = 0;
        let mut k1end: i32 = 0;
        let mut k2start: i32 = 0;
        let mut k2end: i32 = 0;
        for d in 0..max_d {
            if self.diff_timeout.is_some()
                && start_time.elapsed().as_secs_f32() >= self.diff_timeout.unwrap()
            {
                break;
            }

            let d1 = d;
            let mut k1 = -d1 + k1start;
            let mut x1: i32;
            let mut k1_offset: i32;
            let mut k2_offset;
            let mut x2;
            let mut y1;
            // Walk the front path one step.
            while k1 < d1 + 1 - k1end {
                k1_offset = v_offset + k1;
                if k1 == -d1
                    || (k1 != d1 && v1[k1_offset as usize - 1] < v1[k1_offset as usize + 1])
                {
                    x1 = v1[k1_offset as usize + 1];
                } else {
                    x1 = v1[k1_offset as usize - 1] + 1;
                }
                y1 = x1 - k1;
                while x1 < text1_length && y1 < text2_length {
                    let i1 = if x1 < 0 { text1_length + x1 } else { x1 };
                    let i2 = if y1 < 0 { text2_length + y1 } else { y1 };
                    if char1[i1 as usize] != char2[i2 as usize] {
                        break;
                    }
                    x1 += 1;
                    y1 += 1;
                }
                v1[k1_offset as usize] = x1;
                if x1 > text1_length {
                    // Ran off the right of the graph.
                    k1end += 2;
                } else if y1 > text2_length {
                    // Ran off the bottom of the graph.
                    k1start += 2;
                } else if front != 0 {
                    k2_offset = v_offset + delta - k1;
                    if k2_offset >= 0 && k2_offset < v_length && v2[k2_offset as usize] != -1 {
                        // Mirror x2 onto top-left coordinate system.
                        x2 = text1_length - v2[k2_offset as usize];
                        if x1 >= x2 {
                            // Overlap detected.
                            return self.diff_bisect_split(char1, char2, x1, y1, start_time);
                        }
                    }
                }
                k1 += 2;
            }
            let mut k2 = -d1 + k2start;
            let mut y2;
            // Walk the reverse path one step.
            while k2 < d1 + 1 - k2end {
                k2_offset = v_offset + k2;
                if k2 == -d1
                    || (k2 != d1 && v2[k2_offset as usize - 1] < v2[k2_offset as usize + 1])
                {
                    x2 = v2[k2_offset as usize + 1];
                } else {
                    x2 = v2[k2_offset as usize - 1] + 1;
                }
                y2 = x2 - k2;
                while x2 < text1_length && y2 < text2_length {
                    let i1 = if text1_length - x2 > 0 {
                        text1_length - x2 - 1
                    } else {
                        x2 + 1
                    };
                    let i2 = if text2_length - y2 > 0 {
                        text2_length - y2 - 1
                    } else {
                        y2 + 1
                    };
                    if char1[i1 as usize] != char2[i2 as usize] {
                        break;
                    }
                    x2 += 1;
                    y2 += 1;
                }
                v2[k2_offset as usize] = x2;
                if x2 > text1_length {
                    // Ran off the left of the graph.
                    k2end += 2;
                } else if y2 > text2_length {
                    // Ran off the top of the graph.
                    k2start += 2;
                } else if front == 0 {
                    k1_offset = v_offset + delta - k2;
                    if k1_offset >= 0 && k1_offset < v_length && v1[k1_offset as usize] != -1 {
                        x1 = v1[k1_offset as usize];
                        y1 = v_offset + x1 - k1_offset;
                        // Mirror x2 onto top-left coordinate system.
                        x2 = text1_length - x2;
                        if x1 >= x2 {
                            // Overlap detected.
                            return self.diff_bisect_split(char1, char2, x1, y1, start_time);
                        }
                    }
                }
                k2 += 2;
            }
        }
        // number of diffs equals number of characters, no commonality at all.
        vec![
            Diff::Delete(char1.iter().collect()),
            Diff::Add(char2.iter().collect()),
        ]
    }

    /// Given the location of the 'middle snake', split the diff in two parts
    /// and recurse.
    ///
    /// Args:
    ///     text1: Old text1 to be diffed.
    ///     text2: New text1 to be diffed.
    ///     x: Index of split point in text1.
    ///     y: Index of split point in text2.
    ///
    /// Returns:
    ///         Vector of diffs as changes.
    fn diff_bisect_split(
        &self,
        text1: &[char],
        text2: &[char],
        x: i32,
        y: i32,
        start_time: Instant,
    ) -> Vec<Diff> {
        let text1a: String = text1[..(x as usize)].iter().collect();
        let text2a: String = text2[..(y as usize)].iter().collect();
        let text1b: String = text1[(x as usize)..].iter().collect();
        let text2b: String = text2[(y as usize)..].iter().collect();

        // Compute both diffs serially.
        let mut diffs =
            self.diff_main_internal(text1a.as_str(), text2a.as_str(), false, start_time);
        let mut diffsb =
            self.diff_main_internal(text1b.as_str(), text2b.as_str(), false, start_time);
        diffs.append(&mut diffsb);
        diffs
    }

    /// Split two texts into an array of strings.  Reduce the texts to a string
    /// of hashes where each Unicode character represents one word.
    ///
    /// Args:
    ///     text1: First chars.
    ///     text2: Second chars.
    ///
    /// Returns:
    ///     Three element tuple, containing the encoded text1, the encoded text2 and
    ///     the array of unique strings.  The zeroth element of the array of unique
    ///     strings is intentionally blank.
    pub fn diff_words_tochars(&self, text1: &str, text2: &str) -> (String, String, Vec<String>) {
        let mut wordarray: Vec<String> = vec!["".to_string()];
        let mut wordhash: HashMap<String, u32> = HashMap::new();
        let chars1 = self.diff_words_tochars_munge(text1, &mut wordarray, &mut wordhash);
        let dmp = Dmp::default();
        let chars2 = dmp.diff_words_tochars_munge(text2, &mut wordarray, &mut wordhash);
        (chars1, chars2, wordarray)
    }

    /// Split a text into an array of strings.  Reduce the texts to a string
    /// of hashes where each Unicode character represents one word.
    /// Modifies wordarray and wordhash through being a closure.
    ///
    /// Args:
    ///     text: chars to encode.
    ///
    /// Returns:
    ///     Encoded string.
    pub fn diff_words_tochars_munge(
        &self,
        text: &str,
        wordarray: &mut Vec<String>,
        wordhash: &mut HashMap<String, u32>,
    ) -> String {
        let mut chars = "".to_string();

        let re = Regex::new(r"[\s\n\r]").unwrap();
        let mut prev_end: usize = 0;
        for part in re.find_iter(text) {
            if prev_end < part.start() {
                let word = &text[prev_end..part.start()];
                chars += &self.make_token_dict(word, wordarray, wordhash);
            }
            let word = &text[part.start()..part.end()];
            chars += &self.make_token_dict(word, wordarray, wordhash);
            prev_end = part.end();
        }
        if prev_end < text.len() {
            let word = &text[prev_end..text.len()];
            chars += &self.make_token_dict(word, wordarray, wordhash);
        }
        chars
    }

    fn make_token_dict(
        &self,
        word: &str,
        wordarray: &mut Vec<String>,
        wordhash: &mut HashMap<String, u32>,
    ) -> String {
        if !wordhash.contains_key(word) {
            wordarray.push(word.to_string());
            wordhash.insert(word.to_string(), wordarray.len() as u32 - 1);
        }
        char::from_u32(wordhash[word]).unwrap().to_string()
    }

    /// Split two texts into an array of strings.  Reduce the texts to a string
    /// of hashes where each Unicode character represents one line.
    ///
    /// Args:
    ///     text1: First chars.
    ///     text2: Second chars.
    ///
    /// Returns:
    ///     Three element tuple, containing the encoded text1, the encoded text2 and
    ///     the array of unique strings.  The zeroth element of the array of unique
    ///     strings is intentionally blank.
    pub fn diff_lines_tochars(
        &self,
        text1: &[char],
        text2: &[char],
    ) -> (String, String, Vec<String>) {
        let mut linearray: Vec<String> = vec!["".to_string()];
        let mut linehash: HashMap<String, i32> = HashMap::new();
        let chars1 = self.diff_lines_tochars_munge(text1, &mut linearray, &mut linehash);
        let dmp = Dmp::default();
        let chars2 = dmp.diff_lines_tochars_munge(text2, &mut linearray, &mut linehash);
        (chars1, chars2, linearray)
    }

    /// Split a text into an array of strings.  Reduce the texts to a string
    /// of hashes where each Unicode character represents one line.
    /// Modifies linearray and linehash through being a closure.
    ///
    /// Args:
    ///     text: chars to encode.
    ///
    /// Returns:
    ///     Encoded string.
    pub fn diff_lines_tochars_munge(
        &self,
        text: &[char],
        linearray: &mut Vec<String>,
        linehash: &mut HashMap<String, i32>,
    ) -> String {
        let mut chars = "".to_string();
        // Walk the text, pulling out a substring for each line.
        // text.split('\n') would would temporarily double our memory footprint.
        // Modifying text would create many large strings to garbage collect.
        let mut line_start = 0;
        let mut line_end = -1;
        let mut line: String;
        while line_end < (text.len() as i32 - 1) {
            line_end = find_char('\n', text, line_start as usize);
            if line_end == -1 {
                line_end = text.len() as i32 - 1;
            }
            line = text[line_start as usize..=line_end as usize]
                .iter()
                .collect();
            if linehash.contains_key(&line) {
                if let Some(char1) = char::from_u32(linehash[&line] as u32) {
                    chars.push(char1);
                    line_start = line_end + 1;
                }
            } else {
                let mut u32char = linearray.len() as i32;

                // skip reserved range - U+D800 to U+DFFF
                // unicode code points in this range can't be converted to unicode scalars
                if u32char >= 55296 {
                    u32char += 2048;
                }

                // 1114111 is the biggest unicode scalar, so stop here
                if u32char == 1114111 {
                    line = text[(line_start as usize)..].iter().collect();
                    line_end = text.len() as i32 - 1;
                }

                linearray.push(line.clone());
                linehash.insert(line.clone(), u32char);

                chars.push(char::from_u32(u32char as u32).unwrap());
                line_start = line_end + 1;
            }
        }
        chars
    }

    /// Rehydrate the text in a diff from a string of line hashes to real lines
    /// of text.
    ///
    /// Args:
    ///     diffs: Vector of diffs as changes.
    ///     lineArray: Vector of unique strings.
    pub fn diff_chars_tolines(&self, diffs: &mut [Diff], line_array: &[String]) {
        for diff in diffs.iter_mut() {
            let mut text: String = "".to_string();
            let text1 = diff.text().clone();
            let chars: Vec<char> = text1.chars().collect();
            for j in 0..chars.len() {
                text += line_array[chars[j] as usize].as_str();
            }
            diff.set_text(text);
        }
    }

    /// Determine the common prefix of two chars.
    ///
    /// Args:
    ///     text1: First chars.
    ///     text2: Second chars.
    ///
    /// Returns:
    ///     The number of characters common to the start of each chars.
    pub fn diff_common_prefix(&self, text1: &[char], text2: &[char]) -> usize {
        if text1.is_empty() || text2.is_empty() {
            return 0;
        }
        let pointermax = min(text1.len(), text2.len());
        let mut pointerstart = 0;
        while pointerstart < pointermax {
            if text1[pointerstart] == text2[pointerstart] {
                pointerstart += 1;
            } else {
                return pointerstart;
            }
        }
        pointermax
    }

    /// Determine the common suffix of two strings.
    ///
    /// Args:
    ///     text1: First chars.
    ///     text2: Second chars.
    ///
    /// Returns:
    ///     The number of characters common to the end of each chars.
    pub fn diff_common_suffix(&self, text1: &[char], text2: &[char]) -> usize {
        if text1.is_empty() || text2.is_empty() {
            return 0;
        }
        let mut out_pointer_1 = text1.len().checked_sub(1);
        let mut out_pointer_2 = text2.len().checked_sub(1);
        let mut len = 0;
        while let (Some(pointer_1), Some(pointer_2)) = (out_pointer_1, out_pointer_2) {
            if text1[pointer_1] == text2[pointer_2] {
                len += 1;
            } else {
                break;
            }
            out_pointer_1 = pointer_1.checked_sub(1);
            out_pointer_2 = pointer_2.checked_sub(1);
        }
        len
    }

    /// Determine if the suffix of one chars is the prefix of another.
    ///
    /// Args:
    ///     text1 First chars.
    ///     text2 Second chars.
    ///
    /// Returns:
    ///     The number of characters common to the end of the first
    ///     chars and the start of the second chars.
    pub fn diff_common_overlap(&self, text1: &[char], text2: &[char]) -> i32 {
        let text1_length = text1.len();
        let text2_length = text2.len();
        if text1_length == 0 || text2_length == 0 {
            return 0;
        }
        let text1_trunc;
        let text2_trunc;
        let len = min(text1_length as i32, text2_length as i32);

        // Truncate the longer chars.
        if text1.len() > text2.len() {
            text1_trunc = text1[(text1_length - text2_length)..].to_vec();
            text2_trunc = text2[..].to_vec();
        } else {
            text1_trunc = text1[..].to_vec();
            text2_trunc = text2[0..text1_length].to_vec();
        }
        let mut best = 0;
        let mut length = 1;
        // Quick check for the worst case.
        if text1_trunc == text2_trunc {
            return len;
        }
        /*Start by looking for a single character match
        and increase length until no match is found.
        Performance analysis: https://neil.fraser.name/news/2010/11/04/ */
        loop {
            let patern = text1_trunc[(len as usize - length)..(len as usize)].to_vec();
            let found = self.kmp(&text2_trunc, &patern, 0);
            let Some(found) = found else {
                return best;
            };
            length += found;
            if found == 0 {
                best = length as i32;
                length += 1;
            }
        }
    }

    /// Do the two texts share a substring which is at least half the length of
    /// the longer text?
    /// This speedup can produce non-minimal diffs.
    ///
    /// Args:
    /// text1: First chars.
    /// text2: Second chars.
    ///
    /// Returns:
    /// Five element Vector, containing the prefix of text1, the suffix of text1,
    /// the prefix of text2, the suffix of text2 and the common middle.  Or empty vector
    /// if there was no match.
    pub fn diff_half_match(&self, text1: &[char], text2: &[char]) -> Option<[String; 5]> {
        self.diff_timeout?;

        let (long_text, short_text) = if text1.len() > text2.len() {
            (text1, text2)
        } else {
            (text2, text1)
        };
        let len1 = short_text.len();
        let len2 = long_text.len();
        if len2 < 4 || len1 * 2 < len2 {
            return None;
        }

        //First check if the second quarter is the seed for a half-match.
        // Check again based on the third quarter.
        let hm = match (
            self.diff_half_matchi(long_text, short_text, (len2 + 3) / 4),
            self.diff_half_matchi(long_text, short_text, (len2 + 1) / 2),
        ) {
            (None, None) => return None,
            (None, Some(hm2)) => hm2,
            (Some(hm1), None) => hm1,
            (Some(hm1), Some(hm2)) => {
                // Both matched.  Select the longest.
                if hm1[4].len() > hm2[4].len() {
                    hm1
                } else {
                    hm2
                }
            }
        };
        if text1.len() > text2.len() {
            return Some(hm);
        }
        let [first, second, third, forth, fifth] = hm;
        let hm = [third, forth, first, second, fifth];
        Some(hm)
    }

    /// Does a substring of shorttext exist within longtext such that the
    /// substring is at least half the length of longtext?
    /// Closure, but does not reference any external variables.
    ///
    /// Args:
    ///     longtext: Longer chars.
    ///     shorttext: Shorter chars.
    ///     i: Start index of quarter length substring within longtext.
    ///
    /// Returns:
    ///     Five element vector, containing the prefix of longtext, the suffix of
    ///     longtext, the prefix of shorttext, the suffix of shorttext and the
    ///     common middle.  Or empty vector if there was no match.
    fn diff_half_matchi(
        &self,
        long_text: &[char],
        short_text: &[char],
        i: usize,
    ) -> Option<[String; 5]> {
        let long_len = long_text.len();
        let seed = Vec::from_iter(long_text[i..(i + long_len / 4)].iter().cloned());
        let mut best_common = "".to_string();
        let mut best_longtext_a = "".to_string();
        let mut best_longtext_b = "".to_string();
        let mut best_shorttext_a = "".to_string();
        let mut best_shorttext_b = "".to_string();
        let mut jk = self.kmp(short_text, &seed, 0);
        while let Some(j) = jk {
            let prefix_length = self.diff_common_prefix(&long_text[i..], &short_text[j..]);
            let suffix_length = self.diff_common_suffix(&long_text[..i], &short_text[..j]);
            if best_common.len() < suffix_length + prefix_length {
                best_common = short_text[(j - suffix_length)..(j + prefix_length)]
                    .iter()
                    .collect();
                best_longtext_a = long_text[..(i - suffix_length)].iter().collect();
                best_longtext_b = long_text[(i + prefix_length)..].iter().collect();
                best_shorttext_a = short_text[..(j - suffix_length)].iter().collect();
                best_shorttext_b = short_text[(j + prefix_length)..].iter().collect();
            }
            jk = self.kmp(short_text, &seed, j + 1);
        }
        if best_common.chars().count() * 2 >= long_text.len() {
            return Some([
                best_longtext_a,
                best_longtext_b,
                best_shorttext_a,
                best_shorttext_b,
                best_common,
            ]);
        }
        None
    }
    /// Reduce the number of edits by eliminating semantically trivial
    /// equalities.
    ///
    /// Args:
    ///     diffs: Vectors of diff object.
    pub fn diff_cleanup_semantic(&self, diffs: &mut Vec<Diff>) {
        let mut changes = false;
        let mut equalities: Vec<i32> = vec![]; // Stack of indices where equalities are found.
        let mut last_equality = "".to_string(); // Always equal to diffs[equalities[-1]][1]
        let mut pointer: i32 = 0; // Index of current position.
                                  // Number of chars that changed prior to the equality.
        let mut length_insertions1 = 0;
        let mut length_deletions1 = 0;
        // Number of chars that changed after the equality.
        let mut length_insertions2 = 0;
        let mut length_deletions2 = 0;
        while (pointer as usize) < diffs.len() {
            if let Diff::Keep(txt) = &diffs[pointer as usize] {
                // Equality found.
                equalities.push(pointer);
                length_insertions1 = length_insertions2;
                length_insertions2 = 0;
                length_deletions1 = length_deletions2;
                length_deletions2 = 0;
                last_equality.clone_from(txt);
            } else {
                // An insertion or deletion.
                match &diffs[pointer as usize] {
                    Diff::Add(txt) => {
                        length_insertions2 += txt.len() as i32;
                    }
                    Diff::Delete(txt) => {
                        length_deletions2 += txt.len() as i32;
                        // Eliminate an equality that is smaller or equal to the edits on both
                        // sides of it.
                    }
                    Diff::Keep(_) => unreachable!(),
                }
                let last_equality_len = last_equality.chars().count() as i32;
                if last_equality_len > 0
                    && last_equality_len <= max(length_insertions1, length_deletions1)
                    && last_equality_len <= max(length_insertions2, length_deletions2)
                {
                    // Duplicate record.
                    diffs.insert(
                        equalities[equalities.len() - 1] as usize,
                        Diff::Delete(last_equality.clone()),
                    );
                    // Change second copy to insert.
                    diffs[equalities[equalities.len() - 1] as usize + 1] = Diff::Add(
                        diffs[equalities[equalities.len() - 1] as usize + 1]
                            .text()
                            .clone(),
                    );
                    // Throw away the equality we just deleted.
                    equalities.pop();
                    // Throw away the previous equality (it needs to be reevaluated).
                    if !equalities.is_empty() {
                        equalities.pop();
                    }
                    if !equalities.is_empty() {
                        pointer = equalities[equalities.len() - 1];
                    } else {
                        pointer = -1;
                    }
                    // Reset the counters.
                    length_insertions1 = 0;
                    length_deletions1 = 0;
                    length_insertions2 = 0;
                    length_deletions2 = 0;
                    last_equality = "".to_string();
                    changes = true;
                }
            }
            pointer += 1;
        }
        // Normalize the diff.
        if changes {
            self.diff_cleanup_merge(diffs);
        }
        self.diff_cleanup_semantic_lossless(diffs);

        let mut overlap_length1: i32;
        let mut overlap_length2: i32;
        pointer = 1;
        while (pointer as usize) < diffs.len() {
            if let (Diff::Delete(deletion_txt), Diff::Add(insertion_txt)) =
                (&diffs[pointer as usize - 1], &diffs[pointer as usize])
            {
                let deletion_vec: Vec<char> = deletion_txt.chars().collect();
                let insertion_vec: Vec<char> = insertion_txt.chars().collect();
                overlap_length1 = self.diff_common_overlap(&deletion_vec, &insertion_vec);
                overlap_length2 = self.diff_common_overlap(&insertion_vec, &deletion_vec);
                if overlap_length1 >= overlap_length2 {
                    if (overlap_length1 as f32) >= (deletion_vec.len() as f32 / 2.0)
                        || (overlap_length1 as f32) >= (insertion_vec.len() as f32 / 2.0)
                    {
                        // Overlap found.  Insert an equality and trim the surrounding edits.
                        diffs.insert(
                            pointer as usize,
                            Diff::Keep(
                                insertion_vec[..(overlap_length1 as usize)].iter().collect(),
                            ),
                        );
                        diffs[pointer as usize - 1] = Diff::Delete(
                            deletion_vec[..(deletion_vec.len() - overlap_length1 as usize)]
                                .iter()
                                .collect(),
                        );
                        diffs[pointer as usize + 1] =
                            Diff::Add(insertion_vec[(overlap_length1 as usize)..].iter().collect());
                        pointer += 1;
                    }
                } else if (overlap_length2 as f32) >= (deletion_vec.len() as f32 / 2.0)
                    || (overlap_length2 as f32) >= (insertion_vec.len() as f32 / 2.0)
                {
                    // Reverse overlap found.
                    // Insert an equality and swap and trim the surrounding edits.
                    diffs.insert(
                        pointer as usize,
                        Diff::Keep(deletion_vec[..(overlap_length2 as usize)].iter().collect()),
                    );
                    let insertion_vec_len = insertion_vec.len();
                    diffs[pointer as usize - 1] = Diff::Add(
                        insertion_vec[..(insertion_vec_len - overlap_length2 as usize)]
                            .iter()
                            .collect(),
                    );
                    diffs[pointer as usize + 1] =
                        Diff::Delete(deletion_vec[(overlap_length2 as usize)..].iter().collect());
                    pointer += 1;
                }
                pointer += 1;
            }
            pointer += 1;
        }
    }

    /// Look for single edits surrounded on both sides by equalities
    /// which can be shifted sideways to align the edit to a word boundary.
    /// e.g: The c<ins>at c</ins>ame. -> The <ins>cat </ins>came.
    ///
    /// Args:
    ///     diffs: Vector of diff object.
    pub fn diff_cleanup_semantic_lossless(&self, diffs: &mut Vec<Diff>) {
        let mut pointer = 1;
        let mut equality1;
        let mut equality2;
        let mut edit: String;
        let mut common_offset;
        let mut common_string: String;
        let mut best_equality1;
        let mut best_edit;
        let mut best_equality2;
        let mut best_score;
        let mut score;

        //Intentionally ignore the first and last element (don't need checking).
        while pointer < diffs.len() as i32 - 1 {
            if let (Diff::Keep(prev_txt), Diff::Keep(next_txt)) =
                (&diffs[pointer as usize - 1], &diffs[pointer as usize + 1])
            {
                //  This is a single edit surrounded by equalities.
                equality1 = prev_txt.clone();
                edit = diffs[pointer as usize].text().clone();
                equality2 = next_txt.clone();
                let mut edit_vec: Vec<char> = edit.chars().collect();
                let mut equality1_vec: Vec<char> = equality1.chars().collect();
                let mut equality2_vec: Vec<char> = equality2.chars().collect();

                // First, shift the edit as far left as possible.
                common_offset = self.diff_common_suffix(&equality1_vec, &edit_vec);
                if common_offset != 0 {
                    common_string = edit_vec[(edit_vec.len() - common_offset)..]
                        .iter()
                        .collect();
                    equality1 = equality1_vec[..(equality1_vec.len() - common_offset)]
                        .iter()
                        .collect();
                    let temp7: String = edit_vec[..(edit_vec.len() - common_offset)]
                        .iter()
                        .collect();
                    edit = common_string.clone() + temp7.as_str();
                    equality2 = common_string + equality2.as_str();
                    edit_vec = edit.chars().collect();
                    equality2_vec = equality2.chars().collect();
                    equality1_vec = equality1.chars().collect();
                }
                // Second, step character by character right, looking for the best fit.
                best_equality1 = equality1.clone();
                best_edit = edit;
                best_equality2 = equality2;
                best_score = self.diff_cleanup_semantic_score(&equality1_vec, &edit_vec)
                    + self.diff_cleanup_semantic_score(&edit_vec, &equality2_vec);
                let edit_len = edit_vec.len();
                let mut equality2_len = equality2_vec.len();
                while equality2_len > 0 && edit_len > 0 {
                    if edit_vec[0] != equality2_vec[0] {
                        break;
                    }
                    let ch = edit_vec[0];
                    equality1_vec.push(ch);
                    edit_vec.push(ch);
                    edit_vec = edit_vec[1..].to_vec();
                    equality2_len -= 1;
                    equality2_vec = equality2_vec[1..].to_vec();
                    score = self.diff_cleanup_semantic_score(&equality1_vec, &edit_vec)
                        + self.diff_cleanup_semantic_score(&edit_vec, &equality2_vec);
                    // The >= encourages trailing rather than leading whitespace on edits.
                    if score >= best_score {
                        best_score = score;
                        best_equality1 = equality1_vec[0..].iter().collect();
                        best_edit = edit_vec[..].iter().collect();
                        best_equality2 = equality2_vec[..].iter().collect();
                    }
                }
                if prev_txt != &best_equality1 {
                    // We have an improvement, save it back to the diff.
                    if !best_equality1.is_empty() {
                        diffs[pointer as usize - 1] =
                            diffs[pointer as usize - 1].with_text(best_equality1);
                    } else {
                        diffs.remove(pointer as usize - 1);
                        pointer -= 1;
                    }
                    diffs[pointer as usize] = diffs[pointer as usize].with_text(best_edit);
                    if !best_equality2.is_empty() {
                        diffs[pointer as usize + 1] =
                            diffs[pointer as usize + 1].with_text(best_equality2);
                    } else {
                        diffs.remove(pointer as usize + 1);
                        pointer += 1;
                    }
                }
            }
            pointer += 1;
        }
    }

    /// Given two strings, compute a score representing whether the
    /// internal boundary falls on logical boundaries.
    /// Scores range from 6 (best) to 0 (worst).
    /// Closure, but does not reference any external variables.
    ///
    /// Args:
    ///     one: First chars.
    ///     two: Second chars.
    ///
    /// Returns:
    ///     The score.
    fn diff_cleanup_semantic_score(&self, one: &[char], two: &[char]) -> i32 {
        if one.is_empty() || two.is_empty() {
            // Edges are the best.
            return 6;
        }

        // Each port of this function behaves slightly differently due to
        // subtle differences in each language's definition of things like
        // 'whitespace'.  Since this function's purpose is largely cosmetic,
        // the choice has been made to use each language's native features
        // rather than force total conformity.
        let char1 = one[one.len() - 1];
        let char2 = two[0];
        let nonalphanumeric1: bool = !char1.is_alphanumeric();
        let nonalphanumeric2: bool = !char2.is_alphanumeric();
        let whitespace1: bool = nonalphanumeric1 & char1.is_whitespace();
        let whitespace2: bool = nonalphanumeric2 & char2.is_whitespace();
        let linebreak1: bool = whitespace1 & ((char1 == '\r') | (char1 == '\n'));
        let linebreak2: bool = whitespace2 & ((char2 == '\r') | (char2 == '\n'));
        let mut test1: bool = false;
        let mut test2: bool = false;
        if one.len() > 1 && one[one.len() - 1] == '\n' && one[one.len() - 2] == '\n' {
            test1 = true;
        }
        if one.len() > 2
            && one[one.len() - 1] == '\n'
            && one[one.len() - 3] == '\n'
            && one[one.len() - 2] == '\r'
        {
            test1 = true;
        }
        if two.len() > 1 && two[two.len() - 1] == '\n' && two[two.len() - 2] == '\n' {
            test2 = true;
        }
        if two.len() > 2
            && two[two.len() - 1] == '\n'
            && two[two.len() - 3] == '\n'
            && two[two.len() - 2] == '\r'
        {
            test2 = true;
        }
        let blankline1: bool = linebreak1 & test1;
        let blankline2: bool = linebreak2 & test2;
        if blankline1 || blankline2 {
            // Five points for blank lines.
            return 5;
        }
        if linebreak1 || linebreak2 {
            // Four points for line breaks.
            return 4;
        }
        if nonalphanumeric1 && !whitespace1 && whitespace2 {
            // Three points for end of sentences.
            return 3;
        }
        if whitespace1 || whitespace2 {
            // Two points for whitespace.
            return 2;
        }
        if nonalphanumeric1 || nonalphanumeric2 {
            // One point for non-alphanumeric.
            return 1;
        }
        0
    }

    /// Reduce the number of edits by eliminating operation(ally trivial
    /// equalities.
    ///
    /// Args:
    ///     diffs: Vector of diff object.
    pub fn diff_cleanup_efficiency(&self, diffs: &mut Vec<Diff>) {
        if diffs.is_empty() {
            return;
        }
        let mut changes: bool = false;
        let mut equalities: Vec<i32> = vec![]; //Stack of indices where equalities are found.
        let mut last_equality: String = "".to_string(); // Always equal to diffs[equalities[-1]][1]
        let mut pointer: i32 = 0; // Index of current position.
        let mut pre_ins = false; // Is there an insertion operation() before the last equality.
        let mut pre_del = false; // Is there a deletion operation() before the last equality.
        let mut post_ins = false; // Is there an insertion operation() after the last equality.
        let mut post_del = false; // Is there a deletion operation() after the last equality.
        while (pointer as usize) < diffs.len() {
            if let Diff::Keep(txt) = &diffs[pointer as usize] {
                if txt.len() < self.edit_cost && (post_del || post_ins) {
                    // Candidate found.
                    equalities.push(pointer);
                    pre_ins = post_ins;
                    pre_del = post_del;
                    last_equality.clone_from(txt);
                } else {
                    // Not a candidate, and can never become one.
                    equalities = vec![];
                    last_equality = "".to_string();
                }
                post_ins = false;
                post_del = false;
            } else {
                // An insertion or deletion.
                if let Diff::Delete(_) = diffs[pointer as usize] {
                    post_del = true;
                } else {
                    post_ins = true;
                }

                /*
                Five types to be split:
                <ins>A</ins><del>B</del>XY<ins>C</ins><del>D</del>
                <ins>A</ins>X<ins>C</ins><del>D</del>
                <ins>A</ins><del>B</del>X<ins>C</ins>
                <ins>A</del>X<ins>C</ins><del>D</del>
                <ins>A</ins><del>B</del>X<del>C</del>
                */

                if !last_equality.is_empty()
                    && ((pre_ins && pre_del && post_del && post_ins)
                        || (last_equality.chars().count() < self.edit_cost / 2
                            && (pre_ins as i32
                                + pre_del as i32
                                + post_del as i32
                                + post_ins as i32)
                                == 3))
                {
                    // Duplicate record.
                    diffs.insert(
                        equalities[equalities.len() - 1] as usize,
                        Diff::Delete(last_equality),
                    );
                    // Change second copy to insert.
                    diffs[equalities[equalities.len() - 1] as usize + 1] = Diff::Add(
                        diffs[equalities[equalities.len() - 1] as usize + 1]
                            .text()
                            .clone(),
                    );
                    equalities.pop(); // Throw away the equality we just deleted.
                    last_equality = "".to_string();
                    if pre_ins && pre_del {
                        // No changes made which could affect previous entry, keep going.
                        post_del = true;
                        post_ins = true;
                        equalities = vec![];
                    } else {
                        if !equalities.is_empty() {
                            equalities.pop(); // Throw away the previous equality.
                        }
                        if !equalities.is_empty() {
                            pointer = equalities[equalities.len() - 1];
                        } else {
                            pointer = -1;
                        }
                        post_ins = false;
                        post_del = false;
                    }
                    changes = true;
                }
            }
            pointer += 1;
        }
        if changes {
            self.diff_cleanup_merge(diffs);
        }
    }

    /// Reorder and merge like edit sections.  Merge equalities.
    /// Any edit section can move as long as it doesn't cross an equality.
    ///
    /// Args:
    ///     diffs: vectors of diff object.
    pub fn diff_cleanup_merge(&self, diffs: &mut Vec<Diff>) {
        if diffs.is_empty() {
            return;
        }
        diffs.push(Diff::Keep("".to_string()));
        let mut text_insert: String = "".to_string();
        let mut text_delete: String = "".to_string();
        let mut i: i32 = 0;
        let mut count_insert = 0;
        let mut count_delete = 0;
        while (i as usize) < diffs.len() {
            match &diffs[i as usize] {
                Diff::Delete(txt) => {
                    text_delete += txt;
                    count_delete += 1;
                    i += 1;
                }
                Diff::Add(txt) => {
                    text_insert += txt;
                    count_insert += 1;
                    i += 1;
                }
                Diff::Keep(txt) => {
                    // Upon reaching an equality, check for prior redundancies.
                    let txt = txt.clone();
                    if count_delete + count_insert > 1 {
                        let mut delete_vec: Vec<char> = text_delete.chars().collect();
                        let mut insert_vec: Vec<char> = text_insert.chars().collect();
                        if count_delete > 0 && count_insert > 0 {
                            // Factor out any common prefixies.
                            let commonlength = self.diff_common_prefix(&insert_vec, &delete_vec);
                            if commonlength != 0 {
                                let temp1: String = (&insert_vec)[..commonlength].iter().collect();
                                let x = i - count_delete - count_insert - 1;
                                if x >= 0 && matches!(diffs[x as usize], Diff::Keep(_)) {
                                    diffs[x as usize] = diffs[x as usize].with_text(
                                        diffs[x as usize].text().clone() + temp1.as_str(),
                                    );
                                } else {
                                    diffs.insert(0, Diff::Keep(temp1));
                                    i += 1;
                                }
                                insert_vec = insert_vec[commonlength..].to_vec();
                                delete_vec = delete_vec[commonlength..].to_vec();
                            }

                            // Factor out any common suffixies.
                            let commonlength = self.diff_common_suffix(&insert_vec, &delete_vec);
                            if commonlength != 0 {
                                let temp1: String = (&insert_vec)
                                    [(insert_vec.len() - commonlength)..]
                                    .iter()
                                    .collect();
                                diffs[i as usize] = diffs[i as usize].with_text(temp1 + &txt);
                                insert_vec =
                                    insert_vec[..(insert_vec.len() - commonlength)].to_vec();
                                delete_vec =
                                    delete_vec[..(delete_vec.len() - commonlength)].to_vec();
                            }
                        }

                        // Delete the offending records and add the merged ones.
                        i -= count_delete + count_insert;
                        for _j in 0..(count_delete + count_insert) as usize {
                            diffs.remove(i as usize);
                        }
                        if !delete_vec.is_empty() {
                            diffs.insert(i as usize, Diff::Delete(delete_vec.iter().collect()));
                            i += 1;
                        }
                        if !insert_vec.is_empty() {
                            diffs.insert(i as usize, Diff::Add(insert_vec.iter().collect()));
                            i += 1;
                        }
                        i += 1;
                    } else if i != 0 && matches!(diffs[i as usize - 1], Diff::Keep(_)) {
                        // Merge this equality with the previous one.
                        diffs[i as usize - 1] = diffs[i as usize - 1].with_text(
                            diffs[i as usize - 1].text().clone()
                                + diffs[i as usize].text().as_str(),
                        );
                        diffs.remove(i as usize);
                    } else {
                        i += 1;
                    }
                    count_delete = 0;
                    text_delete = "".to_string();
                    text_insert = "".to_string();
                    count_insert = 0;
                }
            }
        }
        // Remove the dummy entry at the end.
        if diffs[diffs.len() - 1].text().is_empty() {
            diffs.pop();
        }

        /*
        Second pass: look for single edits surrounded on both sides by equalities
        which can be shifted sideways to eliminate an equality.
        e.g: A<ins>BA</ins>C -> <ins>AB</ins>AC
        */
        let mut changes = false;
        i = 1;
        // Intentionally ignore the first and last element (don't need checking).
        while (i as usize) < diffs.len() - 1 {
            if let (Diff::Keep(prev_txt), Diff::Keep(next_txt)) =
                (&diffs[i as usize - 1], &diffs[i as usize + 1])
            {
                // This is a single edit surrounded by equalities.
                let text_vec = diffs[i as usize].text().chars().collect::<Vec<_>>();
                let text1_vec = prev_txt.chars().collect::<Vec<_>>();
                let text2_vec: Vec<char> = next_txt.chars().collect();
                let prev_text = prev_txt.clone();
                let next_text = next_txt.clone();
                if self.endswith(&text_vec, &text1_vec) {
                    // Shift the edit over the previous equality.
                    if !diffs[i as usize - 1].text().is_empty() {
                        let temp1: String = diffs[i as usize - 1].text().clone();
                        let temp2: String = text_vec[..(text_vec.len() - text1_vec.len())]
                            .iter()
                            .collect();
                        diffs[i as usize].set_text(temp1 + temp2.as_str());
                        diffs[i as usize + 1].set_text(prev_text + &next_text);
                    }
                    diffs.remove(i as usize - 1);
                    changes = true;
                } else if self.startswith(&text_vec, &text2_vec) {
                    // Shift the edit over the next equality.
                    diffs[i as usize - 1].set_text(prev_text + &next_text);
                    let temp1: String = text_vec[text2_vec.len()..].iter().collect();
                    diffs[i as usize].set_text(temp1 + &next_text);
                    diffs.remove(i as usize + 1);
                    changes = true;
                }
            }
            i += 1;
        }
        // If shifts were made, the diff needs reordering and another shift sweep.
        if changes {
            self.diff_cleanup_merge(diffs);
        }
    }

    /// It will check if first chars vector is endswith second chars vector or not.
    ///
    /// Args:
    ///     first: First chars,
    ///     second: Secodn chars.
    /// Returns:
    ///     Return true if first chars vector endswith second chars vector, false otherwise.
    fn endswith(&self, first: &[char], second: &[char]) -> bool {
        let mut len1 = first.len();
        let mut len2 = second.len();
        if len1 < len2 {
            return false;
        }
        while len2 > 0 {
            if first[len1 - 1] != second[len2 - 1] {
                return false;
            }
            len1 -= 1;
            len2 -= 1;
        }
        true
    }

    /// It will check if first chars vector is startswith second chars vector or not.
    ///
    /// Args:
    ///     first: First chars,
    ///     second: Secodn chars.
    /// Returns:
    ///     Return true if first chars vector startswith second chars vector, false otherwise.
    fn startswith(&self, first: &[char], second: &[char]) -> bool {
        let len1 = first.len();
        let len2 = second.len();
        if len1 < len2 {
            return false;
        }
        for i in 0..len2 {
            if first[i] != second[i] {
                return false;
            }
        }
        true
    }

    /// loc is a location in text1, compute and return the equivalent location
    /// in text2.  e.g. "The cat" vs "The big cat", 1->1, 5->8
    ///
    /// Args:
    ///     diffs: Vector of diff object.
    ///     loc: Location within text1.
    ///
    /// Returns:
    ///     Location within text2.
    pub fn diff_xindex(&self, diffs: &Vec<Diff>, loc: i32) -> i32 {
        let mut chars1 = 0;
        let mut chars2 = 0;
        let mut last_chars1 = 0;
        let mut last_chars2 = 0;
        let mut lastdiff = Diff::Keep("".to_string());
        let z = 0;
        for diffs_item in diffs {
            if let Diff::Keep(txt) | Diff::Delete(txt) = &diffs_item {
                // Equality or deletion.
                chars1 += txt.len() as i32;
            }
            if let Diff::Keep(txt) | Diff::Add(txt) = &diffs_item {
                // Equality or insertion.
                chars2 += txt.len() as i32;
            }
            if chars1 > loc {
                // Overshot the location.
                lastdiff = diffs_item.with_text(diffs_item.text().clone());
                break;
            }
            last_chars1 = chars1;
            last_chars2 = chars2;
        }
        if matches!(lastdiff, Diff::Delete(_)) && diffs.len() != z {
            // The location was deleted.
            return last_chars2;
        }
        // Add the remaining len(character).
        last_chars2 + (loc - last_chars1)
    }

    /// Compute and return the source text (all equalities and deletions).
    ///
    /// Args:
    ///     diffs: Vectoe of diff object.
    ///
    /// Returns:
    ///     Source text.
    pub fn diff_text1(&self, diffs: &mut Vec<Diff>) -> String {
        let mut text: String = "".to_string();
        for adiff in diffs {
            if let Diff::Keep(txt) | Diff::Delete(txt) = adiff {
                text += txt;
            }
        }
        text
    }

    /// Compute and return the destination text (all equalities and insertions).
    ///
    /// Args:
    ///     diffs: Vector of diff object.
    ///
    /// Returns:
    ///     destination text.
    pub fn diff_text2(&self, diffs: &mut Vec<Diff>) -> String {
        let mut text: String = "".to_string();
        for adiff in diffs {
            if let Diff::Keep(txt) | Diff::Add(txt) = adiff {
                text += txt;
            }
        }
        text
    }

    /// Compute and return the destination text (all equalities and insertions).
    /// Delta offsets are interpreted in u16 code units
    ///
    /// Args:
    ///     text1: Original text
    ///     delta: Text delta
    ///
    /// Returns:
    ///     Destination text
    pub fn diff_text2_from_delta_u16(&self, text1: &str, delta: &str) -> String {
        let text1_u16: Vec<u16> = text1.encode_utf16().collect();
        let mut text2_u16: Vec<u16> = Vec::new();

        let tokens: Vec<&str> = (*delta).split('\t').collect();

        let mut text_offset = 0;
        for token in tokens {
            if token.is_empty() {
                continue;
            }

            let operation = &token[0..1];
            let operation_content = &token[1..];

            if operation == "+" {
                let decoded = percent_decode_u16(operation_content.as_bytes()).unwrap();
                text2_u16.extend(decoded);
            } else {
                let content_length = operation_content.parse::<usize>().unwrap();

                if operation == "=" {
                    let range = text_offset..(content_length + text_offset);
                    text2_u16.extend(&text1_u16[range]);
                }

                text_offset += content_length;
            }
        }

        // we should have consumed all text
        if text1_u16.len() != text_offset {
            panic!("wrong patern or text");
        }

        String::from_utf16(&text2_u16).unwrap()
    }

    /// Compute the Levenshtein distance; the number of inserted, deleted or
    /// substituted characters.
    ///
    /// Args:
    ///     diffs: Vector of diff object.
    ///
    /// Returns:
    ///     Number of changes.
    pub fn diff_levenshtein(&self, diffs: &Vec<Diff>) -> i32 {
        let mut levenshtein = 0;
        let mut insertions = 0;
        let mut deletions = 0;
        for adiff in diffs {
            match &adiff {
                Diff::Add(txt) => {
                    insertions += txt.len();
                }
                Diff::Delete(txt) => {
                    deletions += txt.len();
                }
                Diff::Keep(_) => {
                    // A deletion and an insertion is one substitution.
                    levenshtein += max(insertions as i32, deletions as i32);
                    insertions = 0;
                    deletions = 0;
                }
            }
        }
        levenshtein += max(insertions as i32, deletions as i32);
        levenshtein
    }

    pub fn diff_todelta(&self, diffs: &mut [Diff]) -> String {
        self.diff_todelta_unit(diffs, LengthUnit::UnicodeScalar)
    }

    /// Crush the diff into an encoded string which describes the operation(s
    /// required to transform text1 into text2.
    /// E.g. =3\t-2\t+ing  -> Keep 3 chars, delete 2 chars, insert 'ing'.
    /// Operation(s are tab-separated.  Inserted text is escaped using %xx notation.
    ///
    /// Args:
    ///     diffs: Vector of diff object.
    ///     length_unit: Unit of length.
    ///         For example diff from "🅰🅱" -> "🅱" can have different delta:
    ///         * When operating on unicode scalars delta will be "-1\t=1"
    ///         * For UTF-16 delta will be "-2\t=2"
    ///
    /// Returns:
    ///     Delta text.
    pub fn diff_todelta_unit(&self, diffs: &mut [Diff], length_unit: LengthUnit) -> String {
        let mut text: String = "".to_string();
        let len = diffs.len();
        for (k, diffs_item) in diffs.iter().enumerate() {
            if let Diff::Add(txt) = &diffs_item {
                // High ascii will raise UnicodeDecodeError.  Use Unicode instead.
                let temp5: Vec<char> = vec![
                    '!', '~', '*', '(', ')', ';', '/', '?', ':', '@', '&', '=', '+', '$', ',', '#',
                    ' ', '\'',
                ];
                let temp4: Vec<char> = txt.chars().collect();
                text += "+";
                for temp4_item in &temp4 {
                    let mut is = false;
                    for temp5_item in &temp5 {
                        if *temp5_item == *temp4_item {
                            text.push(*temp4_item);
                            is = true;
                            break;
                        }
                    }
                    if is {
                        continue;
                    }
                    let mut temp6 = "".to_string();
                    temp6.push(*temp4_item);
                    temp6 = utf8_percent_encode(temp6.as_str(), USERINFO_ENCODE_SET).collect();
                    text += temp6.as_str();
                }
            } else {
                if let Diff::Delete(_) = diffs_item {
                    text += "-";
                } else {
                    text += "=";
                }

                let count: usize = match length_unit {
                    LengthUnit::UnicodeScalar => diffs_item.text().chars().count(),
                    LengthUnit::UTF16 => diffs_item.text().encode_utf16().count(),
                };
                text += count.to_string().as_str();
            }

            if k < len - 1 {
                text += "\t";
            }
        }
        text
    }

    pub fn diff_from_delta(&self, text1: &str, delta: &str) -> Vec<Diff> {
        self.diff_from_delta_unit(text1, delta, LengthUnit::UnicodeScalar)
    }

    /// Given the original text1, and an encoded string which describes the
    /// operation(s required to transform text1 into text2, compute the full diff.
    ///
    /// Args:
    ///     text1: Source string for the diff.
    ///     delta: Delta text.
    ///     length_unit: Unit of length used in delta.
    ///
    /// Returns:
    ///     Vector of diff object.
    ///
    /// Raises:
    ///     ValueError: If invalid input.
    pub fn diff_from_delta_unit(
        &self,
        text1: &str,
        delta: &str,
        length_unit: LengthUnit,
    ) -> Vec<Diff> {
        match length_unit {
            LengthUnit::UnicodeScalar => {
                let text = StringScalarView::new(text1);
                self.diff_from_delta_string_view(&text, delta).unwrap()
            }
            LengthUnit::UTF16 => {
                let text = StringUTF16View::new(text1);
                match self.diff_from_delta_string_view(&text, delta) {
                    Ok(diff) => diff,
                    Err(_) => {
                        let text2 = self.diff_text2_from_delta_u16(text1, delta);
                        self.diff_main(text1, &text2, true)
                    }
                }
            }
        }
    }

    fn diff_from_delta_string_view(
        &self,
        text1: &impl StringView,
        delta: &str,
    ) -> Result<Vec<Diff>, Box<dyn Error>> {
        let mut diffs: Vec<Diff> = vec![];
        let tokens: Vec<&str> = (*delta).split('\t').collect();

        let mut text_offset = 0;
        for token in tokens {
            if token.is_empty() {
                continue;
            }

            let operation = &token[0..1];
            let operation_content = &token[1..];

            if operation == "+" {
                let text = percent_decode(operation_content.as_bytes())
                    .decode_utf8()?
                    .to_string();
                diffs.push(Diff::Add(text));
            } else {
                let content_length = operation_content.parse::<usize>().unwrap();
                let range = text_offset..(content_length + text_offset);

                let text = text1.slice(range)?;
                diffs.push(if operation == "=" {
                    Diff::Keep(text)
                } else {
                    Diff::Delete(text)
                });

                text_offset += content_length;
            }
        }

        // we should have consumed all text
        if text1.len() != text_offset {
            panic!("wrong patern or text");
        }

        Ok(diffs)
    }

    /// Locate the best instance of 'pattern' in 'text' near 'loc'.
    ///
    /// Args:
    ///     text: The text to search.
    ///     pattern: The pattern to search for.
    ///     loc: The location to search around.
    ///
    /// Returns:
    ///     Best match index or -1.
    pub fn match_main(&self, text1: &str, patern1: &str, mut loc: i32) -> i32 {
        loc = max(0, min(loc, text1.len() as i32));
        if patern1.is_empty() {
            return loc;
        }
        if text1.is_empty() {
            return -1;
        }
        let text: Vec<char> = (text1.to_string()).chars().collect();
        let patern: Vec<char> = (patern1.to_string()).chars().collect();
        if text == patern {
            // Shortcut (potentially not guaranteed by the algorithm)
            return 0;
        } else if loc as usize + patern.len() <= text.len()
            && text[(loc as usize)..(loc as usize + patern.len())].to_vec() == patern
        {
            // Perfect match at the perfect spot!  (Includes case of null pattern)
            return loc;
        }
        self.match_bitap(&text, &patern, loc)
    }

    /// Locate the best instance of 'pattern' in 'text' near 'loc' using the
    /// Bitap algorithm.
    ///
    /// Args:
    ///     text: The text to search.
    ///     pattern: The pattern to search for.
    ///     loc: The location to search around.
    ///
    /// Returns:
    ///     Best match index or -1.
    pub fn match_bitap(&self, text: &[char], patern: &[char], loc: i32) -> i32 {
        // check for maxbits limit.
        if !(self.match_maxbits == 0 || patern.len() <= self.match_maxbits) {
            panic!("patern too long for this application");
        }
        // Initialise the alphabet.
        let s: HashMap<char, i32> = self.match_alphabet(patern);

        // Highest score beyond which we give up.
        let mut score_threshold: f32 = self.match_threshold;
        // Is there a nearby exact match? (speedup)
        let mut out_best_loc = self.kmp(text, patern, loc as usize);
        if let Some(best_loc) = out_best_loc {
            score_threshold = min1(
                self.match_bitap_score(0, best_loc as i32, loc, patern),
                score_threshold,
            );
            // What about in the other direction? (speedup)
            out_best_loc = self.rkmp(text, patern, loc as usize + patern.len());
            if let Some(best_loc) = out_best_loc {
                score_threshold = min1(
                    score_threshold,
                    self.match_bitap_score(0, best_loc as i32, loc, patern),
                );
            }
        }
        // Initialise the bit arrays.
        let matchmask = 1 << (patern.len() - 1); //>
        let mut best_loc = -1;
        let mut bin_min: i32;
        let mut bin_mid: i32;
        let mut bin_max: i32 = (patern.len() + text.len()) as i32;
        // Empty initialization added to appease pychecker.
        let mut last_rd: Vec<i32> = vec![];
        for d in 0..patern.len() {
            /*
            Scan for the best match each iteration allows for one more error.
            Run a binary search to determine how far from 'loc' we can stray at
            this error level.
            */
            let mut rd: Vec<i32> = vec![];
            bin_min = 0;
            bin_mid = bin_max;
            // Use the result from this iteration as the maximum for the next.
            while bin_min < bin_mid {
                if self.match_bitap_score(d as i32, loc + bin_mid, loc, patern) <= score_threshold {
                    bin_min = bin_mid;
                } else {
                    bin_max = bin_mid;
                }
                bin_mid = bin_min + (bin_max - bin_min) / 2;
            }
            bin_max = bin_mid;
            let mut start = max(1, loc - bin_mid + 1);
            let finish = min(loc + bin_mid, text.len() as i32) + patern.len() as i32;
            rd.resize((finish + 2) as usize, 0);
            rd[(finish + 1) as usize] = (1 << d) - 1; //>
            let mut j = finish;
            while j >= start {
                let char_match: i32;
                if text.len() < j as usize {
                    // Out of range.
                    char_match = 0;
                } else {
                    // Subsequent passes: fuzzy match.
                    match s.get(&(text[j as usize - 1])) {
                        Some(num) => {
                            char_match = *num;
                        }
                        None => {
                            char_match = 0;
                        }
                    }
                }
                if d == 0 {
                    // First pass: exact match.
                    rd[j as usize] = ((rd[j as usize + 1] << 1) | 1) & char_match;
                //>>
                } else {
                    rd[j as usize] = (((rd[j as usize + 1] << 1) | 1) & char_match)
                        | (((last_rd[j as usize + 1] | last_rd[j as usize]) << 1) | 1)
                        | last_rd[j as usize + 1]; //>>>>
                }
                if (rd[j as usize] & matchmask) != 0 {
                    let score: f32 = self.match_bitap_score(d as i32, j - 1, loc, patern);
                    // This match will almost certainly be better than any existing match.
                    // But check anyway.
                    if score <= score_threshold {
                        // Told you so.
                        score_threshold = score;
                        best_loc = j - 1;
                        if best_loc > loc {
                            // When passing loc, don't exceed our current distance from loc.
                            start = max(1, 2 * loc - best_loc);
                        } else {
                            // Already passed loc, downhill from here on in.
                            break;
                        }
                    }
                }
                j -= 1;
            }
            // No hope for a (better) match at greater error levels.
            if self.match_bitap_score(d as i32 + 1, loc, loc, patern) > score_threshold {
                break;
            }
            last_rd = rd;
        }
        best_loc
    }

    /// Compute and return the score for a match with e errors and x location.
    /// Accesses loc and pattern through being a closure.
    ///
    /// Args:
    ///     e: Number of errors in match.
    ///     x: Location of match.
    ///
    /// Returns:
    ///     Overall score for match (0.0 = good, 1.0 = bad).
    pub fn match_bitap_score(&self, e: i32, x: i32, loc: i32, patern: &[char]) -> f32 {
        let accuracy: f32 = (e as f32) / (patern.len() as f32);
        let proximity: i32 = (loc - x).abs();
        if self.match_distance == 0 {
            // Dodge divide by zero error.
            if proximity == 0 {
                return accuracy;
            } else {
                return 1.0;
            }
        }
        accuracy + ((proximity as f32) / (self.match_distance as f32))
    }
    /// Initialise the alphabet for the Bitap algorithm.
    ///
    /// Args:
    ///     pattern: The text to encode.
    ///
    /// Returns:
    ///     Hash of character locations.
    pub fn match_alphabet(&self, patern: &[char]) -> HashMap<char, i32> {
        let mut s: HashMap<char, i32> = HashMap::new();
        for patern_item in patern {
            s.insert(*patern_item, 0);
        }
        for i in 0..patern.len() {
            let ch: char = patern[i];
            let mut temp: i32 = 0;
            if let Some(num) = s.get(&ch) {
                temp = num | (1 << (patern.len() - i - 1)); //>>
            }
            s.insert(ch, temp);
        }
        s
    }

    /// Increase the context until it is unique,
    /// but don't let the pattern expand beyond Match_MaxBits.
    ///
    /// Args:
    ///     patch: The patch to grow.
    ///     text: Source text.
    pub fn patch_add_context(&self, patch: &mut Patch, text: &mut [char]) {
        if text.is_empty() {
            return;
        }
        let mut pattern: Vec<char> = text[patch.start2..(patch.length1 + patch.start2)].to_vec();
        let mut padding: usize = 0;

        // Look for the first and last matches of pattern in text.  If two different
        // matches are found, increase the pattern length.
        let mut rst = 0;
        while self.kmp(text, &pattern, 0) != self.rkmp(text, &pattern, text.len() - 1)
            && pattern.len() < (self.match_maxbits - self.patch_margin * 2)
        {
            padding += self.patch_margin;

            let first = patch
                .start2
                .checked_sub(padding)
                .map(|x| max(0, x))
                .unwrap_or(0);
            pattern = text[first..min(text.len(), patch.start2 + patch.length1 + padding)].to_vec();
            rst += 1;
            if rst > 5 {
                break;
            }
        }
        // Add one chunk for good luck.
        padding += self.patch_margin;

        // Add the prefix.
        let first = patch
            .start2
            .checked_sub(padding)
            .map(|x| max(0, x))
            .unwrap_or(0);
        let prefix: String = text[first..patch.start2].iter().collect();
        let prefix_length = prefix.chars().count();
        if !prefix.is_empty() {
            patch.diffs.insert(0, Diff::Keep(prefix.clone()));
        }

        // Add the suffix.
        let suffix: String = text[(patch.start2 + patch.length1)
            ..min(text.len(), patch.start2 + patch.length1 + padding)]
            .iter()
            .collect();
        let suffix_length = suffix.chars().count();
        if !suffix.is_empty() {
            patch.diffs.push(Diff::Keep(suffix));
        }
        // Roll back the start points.
        patch.start1 -= prefix_length;
        patch.start2 -= prefix_length;
        // Extend lengths.
        patch.length1 += prefix_length + suffix_length;
        patch.length2 += prefix_length + suffix_length;
    }

    /// Compute a list of patches to turn text1 into text2.
    /// compute diffs.
    /// Args:
    ///     text1: First string.
    ///     text2: Second string.
    /// Returns:
    ///     Vector of Patch objects.
    pub fn patch_make1(&self, text1: &str, text2: &str) -> Vec<Patch> {
        let mut diffs: Vec<Diff> = self.diff_main(text1, text2, true);
        if diffs.len() > 2 {
            self.diff_cleanup_semantic(&mut diffs);
            self.diff_cleanup_efficiency(&mut diffs);
        }
        self.patch_make4(text1, &mut diffs)
    }

    /// Compute a list of patches to turn text1 into text2.
    /// Use diffs to compute first text.
    ///
    /// Args:
    ///     diffs: Vector od diff object.
    /// Returns:
    ///     Vector of Patch objects.
    pub fn patch_make2(&self, diffs: &mut Vec<Diff>) -> Vec<Patch> {
        let text1 = self.diff_text1(diffs);
        self.patch_make4(text1.as_str(), diffs)
    }

    /// Compute a list of patches to turn text1 into text2.
    ///
    /// Args:
    ///     text1: First string.
    ///     text2: Second string.
    ///     diffs: Vector of diff.
    ///
    /// Returns:
    ///     Vector of Patch objects.
    pub fn patch_make3(&self, text1: &str, _text2: &str, diffs: &mut [Diff]) -> Vec<Patch> {
        self.patch_make4(text1, diffs)
    }
    /// Compute a list of patches to turn text1 into text2.
    ///
    /// Args:
    ///     text1: First string.
    ///     diffs: Vector of diff object.
    /// Returns:
    ///     Array of Patch objects.
    pub fn patch_make4(&self, text1: &str, diffs: &mut [Diff]) -> Vec<Patch> {
        let mut patches: Vec<Patch> = vec![];
        if diffs.is_empty() {
            return patches; // Get rid of the None case.
        }
        let mut patch: Patch = Patch::new(vec![], 0, 0, 0, 0);
        let mut char_count1 = 0; // Number of characters into the text1 string.
        let mut char_count2 = 0; // Number of characters into the text2 string.
        let mut prepatch: Vec<char> = (text1.to_string()).chars().collect(); // Recreate the patches to determine context info.
        let mut postpatch: Vec<char> = (text1.to_string()).chars().collect();
        for i in 0..diffs.len() {
            if patch.diffs.is_empty() && matches!(diffs[i], Diff::Add(_) | Diff::Delete(_)) {
                // A new patch starts here.
                patch.start1 = char_count1;
                patch.start2 = char_count2;
            }
            match &diffs[i] {
                Diff::Add(txt) => {
                    // Insertion
                    patch.diffs.push(diffs[i].clone());
                    let temp: Vec<char> = postpatch[char_count2..].to_vec();
                    postpatch = postpatch[..char_count2].to_vec();
                    patch.length2 += txt.len();
                    for ch in txt.chars() {
                        postpatch.push(ch);
                    }
                    for ch in temp {
                        postpatch.push(ch);
                    }
                }
                Diff::Delete(txt) => {
                    // Deletion.
                    patch.diffs.push(diffs[i].clone());
                    let temp: Vec<char> = postpatch[(txt.len() + char_count2)..].to_vec();
                    postpatch = postpatch[..char_count2].to_vec();
                    patch.length1 += txt.len();
                    for ch in &temp {
                        postpatch.push(*ch);
                    }
                }
                Diff::Keep(txt) => {
                    if txt.len() <= self.patch_margin * 2
                        && !patch.diffs.is_empty()
                        && i != diffs.len() - 1
                    {
                        // Small equality inside a patch.
                        patch.diffs.push(diffs[i].clone());
                        patch.length1 += txt.len();
                        patch.length2 += txt.len();
                    }

                    // Time for a new patch.
                    if txt.len() >= 2 * self.patch_margin && !patch.diffs.is_empty() {
                        self.patch_add_context(&mut patch, &mut prepatch);
                        patches.push(patch);
                        patch = Patch::new(vec![], 0, 0, 0, 0);
                        prepatch.clone_from(&postpatch);
                        char_count1 = char_count2;
                    }
                }
            }

            // Update the current character count.
            if let Diff::Keep(txt) | Diff::Delete(txt) = &diffs[i] {
                char_count1 += txt.len();
            }
            let temp1: &Vec<char> = &diffs[i].text().chars().collect();
            if let Diff::Keep(_) | Diff::Add(_) = &diffs[i] {
                char_count2 += temp1.len();
            }
        }

        // Pick up the leftover patch if not empty.
        if !patch.diffs.is_empty() {
            self.patch_add_context(&mut patch, &mut prepatch);
            // println!("{:?}", prepatch);
            patches.push(patch);
        }
        patches
    }

    /// Given an Vector of patches, return another Vector that is identical.
    ///
    /// Args:
    ///     patches: Vector of Patch objects.
    ///
    /// Returns:
    ///     Vector of Patch objects.
    pub fn patch_deep_copy(&self, patches: &mut Vec<Patch>) -> Vec<Patch> {
        let mut patches_copy: Vec<Patch> = vec![];
        for patches_item in patches {
            let mut patch_copy = Patch::new(vec![], 0, 0, 0, 0);
            for j in 0..patches_item.diffs.len() {
                let diff_copy =
                    patches_item.diffs[j].with_text(patches_item.diffs[j].text().clone());
                patch_copy.diffs.push(diff_copy);
            }
            patch_copy.start1 = patches_item.start1;
            patch_copy.start2 = patches_item.start2;
            patch_copy.length1 = patches_item.length1;
            patch_copy.length2 = patches_item.length2;
            patches_copy.push(patch_copy);
        }
        patches_copy
    }

    /// Merge a set of patches onto the text.  Return a patched text, as well
    /// as a list of true/false values indicating which patches were applied.
    ///
    /// Args:
    ///     patches: Vector of Patch objects.
    ///     text: Old text.
    ///
    /// Returns:
    ///     Two element Vector, containing the new chars and an Vector of boolean values.
    pub fn patch_apply(
        &self,
        patches: &mut Vec<Patch>,
        source_text: &str,
    ) -> (Vec<char>, Vec<bool>) {
        if patches.is_empty() {
            return (source_text.chars().collect(), vec![]);
        }

        // Deep copy the patches so that no changes are made to originals.
        let mut patches_copy: Vec<Patch> = self.patch_deep_copy(patches);

        let null_padding: Vec<char> = self.patch_add_padding(&mut patches_copy);

        let mut text = null_padding.clone();
        text.extend(source_text.chars());
        text.extend(&null_padding);

        self.patch_splitmax(&mut patches_copy);

        // delta keeps track of the offset between the expected and actual location
        // of the previous patch.  If there are patches expected at positions 10 and
        // 20, but the first patch was found at 12, delta is 2 and the second patch
        // has an effective expected position of 22.
        let mut delta: i32 = 0;
        let mut results: Vec<bool> = vec![false; patches_copy.len()];
        for x in 0..patches_copy.len() {
            let expected_loc: i32 = patches_copy[x].start2 as i32 + delta;
            let text1: Vec<char> = self
                .diff_text1(&mut patches_copy[x].diffs)
                .chars()
                .collect();
            let mut start_loc: i32;
            let mut end_loc = -1;
            if text1.len() > self.match_maxbits {
                // patch_splitMax will only provide an oversized pattern in the case of
                // a monster delete.
                let first: String = (text[..]).iter().collect();
                let second: String = text1[..self.match_maxbits].iter().collect();
                let second1: String = text1[text1.len() - self.match_maxbits..].iter().collect();
                start_loc = self.match_main(first.as_str(), second.as_str(), expected_loc);
                if start_loc != -1 {
                    end_loc = self.match_main(
                        first.as_str(),
                        second1.as_str(),
                        expected_loc + (text1.len() - self.match_maxbits) as i32,
                    );
                    if end_loc == -1 || start_loc >= end_loc {
                        // Can't find valid trailing context.  Drop this patch.
                        start_loc = -1;
                    }
                }
            } else {
                let first: String = text[..].iter().collect();
                let second: String = text1[..].iter().collect();
                start_loc = self.match_main(first.as_str(), second.as_str(), expected_loc);
            }
            if start_loc == -1 {
                // No match found.  :(
                results[x] = false;
                // Subtract the delta for this failed patch from subsequent patches.
                delta -= patches_copy[x].length2 as i32 - patches_copy[x].length1 as i32;
            } else {
                // Found a match.  :)
                results[x] = true;
                delta = start_loc - expected_loc;

                let mut end_index: usize;
                if end_loc == -1 {
                    end_index = start_loc as usize + text1.len();
                } else {
                    end_index = end_loc as usize + self.match_maxbits;
                }
                end_index = std::cmp::min(text.len(), end_index);

                let text2: Vec<char> = text[start_loc as usize..end_index].to_vec();

                if text1 == text2 {
                    // Perfect match, just shove the replacement text in.
                    let temp3: String = text[..start_loc as usize].iter().collect();
                    let temp4 = self.diff_text2(&mut patches_copy[x].diffs);
                    let temp5: String = text[(start_loc as usize + text1.len())..].iter().collect();
                    let temp6 = temp3 + temp4.as_str() + temp5.as_str();
                    text = temp6.chars().collect();
                } else {
                    // Imperfect match.
                    // Run a diff to get a framework of equivalent indices.
                    let temp3: String = text1[..].iter().collect();
                    let temp4: String = text2[..].iter().collect();
                    let mut diffs: Vec<Diff> =
                        self.diff_main(temp3.as_str(), temp4.as_str(), false);
                    if text1.len() > self.match_maxbits
                        && (self.diff_levenshtein(&diffs) as f32 / (text1.len() as f32)
                            > self.patch_delete_threshold)
                    {
                        // The end points match, but the content is unacceptably bad.
                        results[x] = false;
                    } else {
                        self.diff_cleanup_semantic_lossless(&mut diffs);
                        let mut index1: i32 = 0;
                        for y in 0..patches_copy[x].diffs.len() {
                            let mod1 = patches_copy[x].diffs[y].clone();
                            if let Diff::Add(_) | Diff::Delete(_) = &mod1 {
                                let index2: i32 = self.diff_xindex(&diffs, index1);
                                if let Diff::Add(txt) = &mod1 {
                                    // Insertion
                                    let temp3: String =
                                        text[..(start_loc + index2) as usize].iter().collect();
                                    let temp4: String =
                                        text[(start_loc + index2) as usize..].iter().collect();
                                    let temp5 = temp3 + txt + temp4.as_str();
                                    text = temp5.chars().collect();
                                } else if let Diff::Delete(txt) = &mod1 {
                                    // Deletion
                                    let temp3: String =
                                        text[..(start_loc + index2) as usize].iter().collect();
                                    let diffs_text_len = txt.len();
                                    let temp4: String = text[(start_loc
                                        + self.diff_xindex(&diffs, index1 + diffs_text_len as i32))
                                        as usize..]
                                        .iter()
                                        .collect();
                                    let temp5 = temp3 + temp4.as_str();
                                    text = temp5.chars().collect();
                                }
                            }
                            if let Diff::Keep(txt) | Diff::Add(txt) = mod1 {
                                index1 += txt.len() as i32;
                            }
                        }
                    }
                }
            }
        }
        // Strip the padding off.
        text = text[null_padding.len()..(text.len() - null_padding.len())].to_vec();
        (text, results)
    }

    /// Add some padding on text start and end so that edges can match
    /// something.  Intended to be called only from within patch_apply.
    ///
    /// Args:
    ///     patches: Array of Patch objects.
    ///
    /// Returns:
    ///     The padding chars added to each side.
    pub fn patch_add_padding(&self, patches: &mut [Patch]) -> Vec<char> {
        let padding_length = self.patch_margin;
        let mut nullpadding: Vec<char> = vec![];
        for i in 0..padding_length {
            if let Some(ch) = char::from_u32(1 + i as u32) {
                nullpadding.push(ch);
            }
        }

        // Bump all the patches forward.
        for patch in patches.iter_mut() {
            patch.start1 += padding_length;
            patch.start2 += padding_length;
        }
        let mut patch = patches[0].clone();
        let mut diffs = patch.diffs;
        let mut text_len = diffs[0].text().chars().count() as i32;
        if diffs.is_empty() || !matches!(diffs[0], Diff::Keep(_)) {
            // Add nullPadding equality.
            diffs.insert(0, Diff::Keep(nullpadding.clone().iter().collect()));
            patch.start1 -= padding_length; // Should be 0.
            patch.start2 -= padding_length; // Should be 0.
            patch.length1 += padding_length;
            patch.length2 += padding_length;
        } else if padding_length > text_len as usize {
            // Grow first equality.
            let extra_length = padding_length - text_len as usize;
            let mut new_text: String = nullpadding[text_len as usize..].iter().collect();
            new_text += diffs[0].text().as_str();
            diffs[0] = diffs[0].with_text(new_text);
            patch.start1 -= extra_length;
            patch.start2 -= extra_length;
            patch.length1 += extra_length;
            patch.length2 += extra_length;
        }

        // Add some padding on end of last diff.
        patch.diffs = diffs;
        patches[0] = patch;
        patch = patches[patches.len() - 1].clone();
        diffs = patch.diffs;
        text_len = diffs[diffs.len() - 1].text().chars().count() as i32;
        if diffs.is_empty() || !matches!(diffs[diffs.len() - 1], Diff::Keep(_)) {
            // Add nullPadding equality.
            diffs.push(Diff::Keep(nullpadding.clone().iter().collect()));
            patch.length1 += padding_length;
            patch.length2 += padding_length;
        } else if padding_length > text_len as usize {
            // Grow last equality.
            let extra_length = padding_length - text_len as usize;
            let mut new_text: String = nullpadding[..extra_length].iter().collect();
            let diffs_len = diffs.len();
            new_text = diffs[diffs_len - 1].text().clone() + new_text.as_str();
            diffs[diffs_len - 1] = diffs[diffs_len - 1].with_text(new_text);
            patch.length1 += extra_length;
            patch.length2 += extra_length;
        }
        patch.diffs = diffs;
        let patches_len = patches.len();
        patches[patches_len - 1] = patch;
        nullpadding
    }

    /// Look through the patches and break up any which are longer than the
    /// maximum limit of the match algorithm.
    /// Intended to be called only from within patch_apply.
    ///
    /// Args:
    ///     patches: Array of Patch objects.
    pub fn patch_splitmax(&self, patches: &mut Vec<Patch>) {
        let patch_size = self.match_maxbits;
        if patch_size == 0 {
            return;
        }
        let mut x: i32 = 0;
        while (x as usize) < patches.len() {
            if patches[x as usize].length1 <= patch_size {
                x += 1;
                continue;
            }
            // Remove the big old patch.
            let mut bigpatch = patches.remove(x as usize);
            x -= 1;
            let mut start1 = bigpatch.start1;
            let mut start2 = bigpatch.start2;
            let mut precontext: Vec<char> = vec![];
            while !bigpatch.diffs.is_empty() {
                // Create one of several smaller patches.
                let mut patch = Patch::new(vec![], 0, 0, 0, 0);
                let mut empty = true;
                patch.start1 = start1 - precontext.len();
                patch.start2 = start2 - precontext.len();
                if !precontext.is_empty() {
                    patch.length1 = precontext.len();
                    patch.length2 = precontext.len();
                    patch
                        .diffs
                        .push(Diff::Keep(precontext.clone().iter().collect()));
                }
                while !bigpatch.diffs.is_empty() && patch.length1 < (patch_size - self.patch_margin)
                {
                    match &bigpatch.diffs[0] {
                        Diff::Add(txt) => {
                            // Insertions are harmless.
                            patch.length2 += txt.len();
                            start2 += txt.len();
                            patch.diffs.push(bigpatch.diffs[0].clone());
                            bigpatch.diffs.remove(0);
                            empty = false;
                        }
                        Diff::Delete(txt)
                            if patch.diffs.len() == 1
                                && matches!(patch.diffs[0], Diff::Keep(_))
                                && txt.len() > 2 * patch_size =>
                        {
                            // This is a large deletion.  Let it pass in one chunk.
                            patch.length1 += txt.len();
                            start1 += txt.len();
                            empty = false;
                            patch.diffs.push(bigpatch.diffs[0].with_text(txt.clone()));
                            bigpatch.diffs.remove(0);
                        }
                        Diff::Keep(txt) | Diff::Delete(txt) => {
                            // Deletion or equality.  Only take as much as we can stomach.
                            let diff_text_len: i32 = txt.len() as i32;
                            let diff_text: Vec<char> = txt.chars().collect();
                            let diff_text = diff_text[..min(
                                diff_text_len as usize,
                                patch_size - patch.length1 - self.patch_margin,
                            ) as usize]
                                .to_vec();
                            patch.length1 += diff_text.len();
                            start1 += diff_text.len();
                            if let Diff::Keep(_) = bigpatch.diffs[0] {
                                patch.length2 += diff_text.len();
                                start2 += diff_text.len();
                            } else {
                                empty = false;
                            }
                            patch.diffs.push(
                                bigpatch.diffs[0].with_text(diff_text.clone().iter().collect()),
                            );
                            let temp: String = diff_text[..].iter().collect();
                            if &temp == txt {
                                bigpatch.diffs.remove(0);
                            } else {
                                let temp1: Vec<char> = txt.chars().collect();
                                bigpatch.diffs[0]
                                    .set_text(temp1[diff_text.len()..].iter().collect());
                            }
                        }
                    }
                }
                // Compute the head context for the next patch.
                precontext = self.diff_text2(&mut patch.diffs).chars().collect();
                precontext = precontext
                    [(precontext.len() - min(self.patch_margin, precontext.len()))..]
                    .to_vec();
                // Append the end context for this patch.
                let postcontext = if self.diff_text1(&mut bigpatch.diffs).chars().count()
                    > self.patch_margin
                {
                    let temp: Vec<char> = self.diff_text1(&mut bigpatch.diffs).chars().collect();
                    temp[..self.patch_margin].iter().collect()
                } else {
                    self.diff_text1(&mut bigpatch.diffs)
                };
                let postcontext_len = postcontext.chars().count() as i32;
                if !postcontext.is_empty() {
                    patch.length1 += postcontext_len as usize;
                    patch.length2 += postcontext_len as usize;
                    if !patch.diffs.is_empty()
                        && matches!(patch.diffs[patch.diffs.len() - 1], Diff::Keep(_))
                    {
                        let len = patch.diffs.len();
                        patch.diffs[len - 1].append_text(&postcontext);
                    } else {
                        patch.diffs.push(Diff::Keep(postcontext));
                    }
                }
                if !empty {
                    x += 1;
                    patches.insert(x as usize, patch);
                }
            }
            x += 1;
        }
    }

    /// Take a list of patches and return a textual representation.
    ///
    /// Args:
    ///     patches: Vector of Patch objects.
    ///
    /// Returns:
    ///     Text representation of patches.
    pub fn patch_to_text(&self, patches: &mut Vec<Patch>) -> String {
        let mut text: String = "".to_string();
        for patches_item in patches {
            text += (patches_item.to_string()).as_str();
        }
        text
    }

    /// Parse a textual representation of patches and return a list of patch
    /// objects.
    ///
    /// Args:
    ///     textline: Text representation of patches.
    ///
    /// Returns:
    ///     Vector of Patch objects.
    ///
    /// Raises:
    ///     ValueError: If invalid input.
    pub fn patch_from_text(&self, textline: String) -> Vec<Patch> {
        let text: Vec<String> = textline.split("@@ ").map(|x| x.to_string()).collect();
        let mut patches: Vec<Patch> = vec![];
        for (i, text_item) in text.iter().enumerate() {
            if text_item.is_empty() {
                if i == 0 {
                    continue;
                }
                panic!("wrong patch string");
            }
            patches.push(self.patch1_from_text(text_item.clone()));
        }
        patches
    }

    pub fn patch1_from_text(&self, textline: String) -> Patch {
        let text: Vec<String> = textline.split('\n').map(|x| x.to_string()).collect();
        let mut text_vec: Vec<char> = text[0].chars().collect();
        if text_vec.len() < 8
            || text_vec[text_vec.len() - 1] != '@'
            || text_vec[text_vec.len() - 2] != '@'
        {
            panic!("Invalid patch string");
        }
        let mut patch = Patch::new(vec![], 0, 0, 0, 0);
        let mut i = 0;
        let mut temp: i32 = 0;
        while i < text_vec.len() {
            if text_vec[i] < '0' || text_vec[i] > '9' {
                i += 1;
                continue;
            }
            if (temp == 1 || temp == 3) && text_vec[i - 1] != ',' {
                temp += 1;
            }
            let mut s = "".to_string();
            while i < text_vec.len() && text_vec[i] >= '0' && text_vec[i] <= '9' {
                s.push(text_vec[i]);
                i += 1;
            }
            if temp == 0 {
                patch.start1 = s.parse::<usize>().unwrap().saturating_sub(1);
                temp += 1;
            } else if temp == 1 {
                patch.length1 = s.parse().unwrap();
                temp += 1;
            } else if temp == 2 {
                patch.start2 = s.parse::<usize>().unwrap().saturating_sub(1);
                temp += 1;
            } else if temp == 3 {
                patch.length2 = s.parse().unwrap();
                temp += 1;
            } else {
                panic!("Invalid patch string");
            }
            i += 1;
        }
        patch.length1 = 0;
        patch.length2 = 0;
        for text_item in text.iter().take(text.len() - 1).skip(1) {
            text_vec = text_item.chars().collect();
            if text_vec[0] == '+' {
                // Insertion.
                let mut temp6: String = text_vec[1..].iter().collect();
                temp6 = percent_decode(temp6.as_bytes())
                    .decode_utf8()
                    .unwrap()
                    .to_string();
                patch.length2 += temp6.chars().count();
                patch.diffs.push(Diff::Add(temp6));
            } else if text_vec[0] == '-' {
                // Deletion.
                let mut temp6: String = text_vec[1..].iter().collect();
                temp6 = percent_decode(temp6.as_bytes())
                    .decode_utf8()
                    .unwrap()
                    .to_string();
                patch.length1 += temp6.chars().count();
                patch.diffs.push(Diff::Delete(temp6));
            } else if text_vec[0] == ' ' {
                // Minor equality.
                let mut temp6: String = text_vec[1..].iter().collect();
                temp6 = percent_decode(temp6.as_bytes())
                    .decode_utf8()
                    .unwrap()
                    .to_string();
                patch.length1 += temp6.chars().count();
                patch.length2 += temp6.chars().count();
                patch.diffs.push(Diff::Keep(temp6));
            } else {
                panic!("wrong patch string");
            }
        }
        patch
    }
}

impl Display for Patch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Convert patch to string.
        let mut text = "@@ -".to_string();
        let mut start1: u32 = (self.start1 + 1) as u32;
        if self.length1 == 0 && start1 == 1 {
            start1 -= 1;
        }
        text += start1.to_string().as_str();
        if self.length1 > 0 || start1 == 0 {
            text += ",";
            let length1: u32 = self.length1 as u32;
            text += length1.to_string().as_str();
        }
        text += " +";
        let mut start2: u32 = (self.start2 + 1) as u32;
        if self.length2 == 0 && start2 == 1 {
            start2 -= 1;
        }
        text += start2.to_string().as_str();
        if self.length2 > 0 || start2 == 0 {
            text += ",";
            let length2: u32 = self.length2 as u32;
            text += length2.to_string().as_str();
        }
        text += " @@\n";
        for i in 0..self.diffs.len() {
            let (ch, txt) = match &self.diffs[i] {
                Diff::Keep(txt) => (' ', txt),
                Diff::Delete(txt) => ('-', txt),
                Diff::Add(txt) => ('+', txt),
            };
            text.push(ch);
            let text_vec: Vec<char> = txt.chars().collect();
            let temp5: Vec<char> = vec![
                '!', '~', '*', '(', ')', ';', '/', '?', ':', '@', '&', '=', '+', '$', ',', '#',
                ' ', '\'',
            ];
            for text_vec_item in &text_vec {
                let mut is: bool = false;
                for temp5_item in &temp5 {
                    if *text_vec_item == *temp5_item {
                        is = true;
                    }
                }
                if is {
                    text.push(*text_vec_item);
                    continue;
                } else if *text_vec_item == '%' {
                    text += "%25";
                    continue;
                }
                let mut temp6: String = "".to_string();
                temp6.push(*text_vec_item);
                temp6 = utf8_percent_encode(temp6.as_str(), USERINFO_ENCODE_SET).collect();
                text += temp6.as_str();
            }
            text += "\n";
        }
        write!(f, "{text}")
    }
}
