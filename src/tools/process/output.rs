#[derive(Default)]
pub(super) struct OutputBuffer {
    /// Bounded head+tail summary of the combined stdout/stderr byte stream.
    /// Once truncation begins, `retained[..head_len]` maps to logical bytes
    /// `[0, head_len)`, while `retained[head_len..]` maps to
    /// `[tail_start, total_bytes)`. Keeping those logical ranges explicit is
    /// required for truthful replay cursors.
    pub(super) retained: Vec<u8>,
    pub(super) total_bytes: usize,
    head_len: usize,
    tail_start: usize,
    /// Highest stream offset ever rendered into a tool response. Explicit
    /// cursors below this value replay buffered history instead of advancing.
    pub(super) delivered: usize,
    pub(super) truncated: bool,
}

impl OutputBuffer {
    pub(super) fn append(&mut self, bytes: &[u8], limit: usize) {
        if bytes.is_empty() {
            return;
        }
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        if self.total_bytes <= limit {
            self.retained.extend_from_slice(bytes);
            self.head_len = 0;
            self.tail_start = 0;
        } else if limit > 0 {
            let head_len = limit / 2;
            let mut head = if self.truncated {
                self.retained[..self.head_len.min(self.retained.len())].to_vec()
            } else {
                let requested_head_end = head_len.min(self.retained.len());
                let safe_head_end =
                    retreat_if_inside_valid_utf8(&self.retained, requested_head_end);
                self.retained[..safe_head_end].to_vec()
            };
            if head.len() > head_len {
                head.truncate(head_len);
            }
            let incomplete_head = incomplete_utf8_suffix_len(&head);
            if incomplete_head > 0 {
                head.truncate(head.len() - incomplete_head);
            }
            // If UTF-8 alignment shortened the preferred head, donate that spare
            // capacity to the tail so a small buffer can still retain a complete
            // multibyte scalar instead of dropping both halves of it.
            let tail_len = limit.saturating_sub(head.len());

            let mut tail = if self.truncated {
                self.retained[self.head_len.min(self.retained.len())..].to_vec()
            } else {
                self.retained[head.len().min(self.retained.len())..].to_vec()
            };
            tail.extend_from_slice(bytes);
            if tail.len() > tail_len {
                let requested_tail_start = tail.len() - tail_len;
                let safe_tail_start = advance_if_inside_valid_utf8(&tail, requested_tail_start);
                tail.drain(..safe_tail_start);
            }

            self.head_len = head.len();
            self.tail_start = self.total_bytes.saturating_sub(tail.len());
            self.retained = head;
            self.retained.extend_from_slice(&tail);
        } else {
            self.retained.clear();
            self.head_len = 0;
            self.tail_start = self.total_bytes;
        }
        self.truncated |= self.total_bytes > limit;
    }

    /// Render the stream window beginning at `requested` (or just after the
    /// last delivered byte) as text. Returns
    /// `(text, start_offset, next_offset, truncated_ever)`.
    ///
    /// Rendering never consumes bytes: the caller decides whether to continue
    /// from `next_offset` or replay an older cursor after a lost response.
    /// Bytes that fell out of the bounded window are disclosed with an
    /// omission marker instead of being silently skipped.
    pub(super) fn render_window(
        &mut self,
        requested: Option<usize>,
        stream_finished: bool,
    ) -> (String, usize, usize, bool) {
        let raw_tail = if self.truncated {
            &self.retained[self.head_len.min(self.retained.len())..]
        } else {
            &[]
        };
        let tail_skip = if self.truncated {
            leading_utf8_continuation_len(raw_tail)
        } else {
            0
        };
        let render_tail_start = self.tail_start.saturating_add(tail_skip);
        let next = if stream_finished {
            self.total_bytes
        } else {
            self.total_bytes
                .saturating_sub(incomplete_utf8_suffix_len(if !self.truncated {
                    &self.retained
                } else {
                    raw_tail
                }))
        };
        let requested_cursor = requested.unwrap_or(self.delivered).min(next);
        let cursor = self.safe_render_cursor(requested_cursor, next, tail_skip, render_tail_start);
        let boundary_omitted = cursor.saturating_sub(requested_cursor);
        let boundary_prefix = || {
            (boundary_omitted > 0)
                .then(|| format!("[... {boundary_omitted} UTF-8 boundary bytes omitted ...]\n\n"))
        };
        let (mut text, start) = if !self.truncated {
            (
                String::from_utf8_lossy(
                    &self.retained[cursor.min(self.retained.len())..next.min(self.retained.len())],
                )
                .into_owned(),
                cursor,
            )
        } else if cursor < self.head_len {
            let mut text =
                String::from_utf8_lossy(&self.retained[cursor..self.head_len]).into_owned();
            let omitted = render_tail_start.saturating_sub(self.head_len);
            if omitted > 0 {
                text.push_str(&format!(
                    "\n\n[... {omitted} buffered bytes omitted ...]\n\n"
                ));
            }
            let tail_end = next.saturating_sub(render_tail_start);
            let tail = &self.retained[self.head_len + tail_skip..];
            text.push_str(&String::from_utf8_lossy(&tail[..tail_end.min(tail.len())]));
            (text, cursor)
        } else if cursor < render_tail_start {
            let omitted = render_tail_start - cursor;
            let mut text = format!("[... {omitted} buffered bytes omitted ...]\n\n");
            let tail_end = next.saturating_sub(render_tail_start);
            let tail = &self.retained[self.head_len + tail_skip..];
            text.push_str(&String::from_utf8_lossy(&tail[..tail_end.min(tail.len())]));
            // The first renderable retained byte can be later than tail_start
            // when the raw tail begins inside a UTF-8 scalar.
            (text, render_tail_start)
        } else {
            let tail_offset = cursor.saturating_sub(render_tail_start);
            let tail_end = next.saturating_sub(render_tail_start);
            let tail = &self.retained[self.head_len + tail_skip..];
            (
                String::from_utf8_lossy(
                    &tail[tail_offset.min(tail.len())..tail_end.min(tail.len())],
                )
                .into_owned(),
                cursor,
            )
        };
        if let Some(prefix) = boundary_prefix() {
            text.insert_str(0, &prefix);
        }
        self.delivered = self.delivered.max(next);
        (text, start, next, self.truncated)
    }

    fn safe_render_cursor(
        &self,
        cursor: usize,
        next: usize,
        tail_skip: usize,
        render_tail_start: usize,
    ) -> usize {
        if cursor >= next {
            return cursor;
        }
        if !self.truncated {
            return advance_if_inside_valid_utf8(&self.retained, cursor).min(next);
        }
        if cursor < self.head_len {
            return advance_if_inside_valid_utf8(
                &self.retained[..self.head_len.min(self.retained.len())],
                cursor,
            )
            .min(self.head_len)
            .min(next);
        }
        if cursor >= render_tail_start {
            let tail = &self.retained[self
                .head_len
                .saturating_add(tail_skip)
                .min(self.retained.len())..];
            let local = cursor.saturating_sub(render_tail_start).min(tail.len());
            return self
                .tail_start
                .saturating_add(tail_skip)
                .saturating_add(advance_if_inside_valid_utf8(tail, local))
                .min(next);
        }
        cursor
    }
}

fn valid_utf8_scalar_containing(bytes: &[u8], boundary: usize) -> Option<(usize, usize)> {
    if boundary == 0 || boundary >= bytes.len() {
        return None;
    }
    let first = boundary.saturating_sub(3);
    for start in (first..boundary).rev() {
        let lead = bytes[start];
        let expected = match lead {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => continue,
        };
        let end = start.saturating_add(expected);
        if start < boundary
            && boundary < end
            && end <= bytes.len()
            && std::str::from_utf8(&bytes[start..end]).is_ok()
        {
            return Some((start, end));
        }
    }
    None
}

fn retreat_if_inside_valid_utf8(bytes: &[u8], boundary: usize) -> usize {
    valid_utf8_scalar_containing(bytes, boundary)
        .map(|(start, _)| start)
        .unwrap_or(boundary)
}

fn advance_if_inside_valid_utf8(bytes: &[u8], boundary: usize) -> usize {
    valid_utf8_scalar_containing(bytes, boundary)
        .map(|(_, end)| end)
        .unwrap_or(boundary)
}

fn leading_utf8_continuation_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take(3)
        .take_while(|byte| **byte & 0b1100_0000 == 0b1000_0000)
        .count()
}

/// Return the number of trailing bytes that are a valid prefix of one UTF-8
/// scalar value but do not yet form the complete value. Streaming output can
/// split a code point across independent pipe reads, so these bytes must stay
/// buffered until the next chunk arrives instead of being rendered as U+FFFD
/// and permanently advancing the byte cursor past them.
fn incomplete_utf8_suffix_len(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }

    let mut start = bytes.len() - 1;
    let mut continuation_count = 0usize;
    while start > 0 && bytes[start] & 0b1100_0000 == 0b1000_0000 && continuation_count < 3 {
        start -= 1;
        continuation_count += 1;
    }

    let lead = bytes[start];
    let expected = match lead {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return 0,
    };
    let available = bytes.len() - start;
    if available >= expected {
        return 0;
    }
    if bytes[start + 1..]
        .iter()
        .any(|byte| byte & 0b1100_0000 != 0b1000_0000)
    {
        return 0;
    }
    if let Some(second) = bytes.get(start + 1).copied() {
        let valid_second = match lead {
            0xE0 => (0xA0..=0xBF).contains(&second),
            0xED => (0x80..=0x9F).contains(&second),
            0xF0 => (0x90..=0xBF).contains(&second),
            0xF4 => (0x80..=0x8F).contains(&second),
            _ => (0x80..=0xBF).contains(&second),
        };
        if !valid_second {
            return 0;
        }
    }
    available
}

pub(super) fn token_window(text: String, max_tokens: Option<usize>) -> (String, Option<usize>) {
    let Some(max_tokens) = max_tokens.filter(|value| *value > 0) else {
        return (text, None);
    };
    // Codex approximates tokens from JavaScript string length. UTF-16 code
    // units preserve that behavior for astral Unicode instead of undercounting
    // every non-BMP character as one Rust `char`.
    let max_units = max_tokens.saturating_mul(4);
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max_units {
        return (text, None);
    }
    let original = units.len().div_ceil(4);
    let head_units = max_units / 2;
    let tail_units = max_units.saturating_sub(head_units);
    let mut used_head = 0usize;
    let mut head_end = 0usize;
    for (index, character) in text.char_indices() {
        let width = character.len_utf16();
        if used_head.saturating_add(width) > head_units {
            break;
        }
        used_head += width;
        head_end = index + character.len_utf8();
    }
    let mut used_tail = 0usize;
    let mut tail_start = text.len();
    for (index, character) in text.char_indices().rev() {
        let width = character.len_utf16();
        if used_tail.saturating_add(width) > tail_units {
            break;
        }
        used_tail += width;
        tail_start = index;
    }
    tail_start = tail_start.max(head_end);
    let value = format!(
        "{}\n\n[... {} UTF-16 code units omitted ...]\n\n{}",
        &text[..head_end],
        units
            .len()
            .saturating_sub(used_head.saturating_add(used_tail)),
        &text[tail_start..]
    );
    (value, Some(original))
}
