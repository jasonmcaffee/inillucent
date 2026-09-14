//! Overflow chains: reading a payload that did not fit on its page.
//!
//! Invariant: a chain is walked with a bound. Every overflow page holds a
//! four-byte pointer to the next one, and a corrupt file can make that pointer
//! point back at a page already visited - a two-page cycle is four bytes of
//! damage. So the reader tracks how many bytes it still needs, refuses a page
//! it has already seen, and refuses to take more hops than the database has
//! pages. Any of the three alone would stop a loop; all three are cheap and
//! each catches a shape the others let through.
//!
//! An overflow page is a four-byte next-page pointer followed by payload, and
//! the payload area is the usable size less those four bytes. The last page in
//! a chain has a zero pointer and is usually not full.

use std::collections::BTreeSet;

use inillucent_base::bytes;
use inillucent_base::error::{corrupt, too_big};
use inillucent_base::ids::PageId;
use inillucent_base::limits::{Limit, Limits};
use inillucent_base::DbResult;

use crate::pager::Pager;
use crate::{alloc, ptrmap};

/// How many bytes of an overflow page are payload.
pub fn payload_per_page(usable: u32) -> DbResult<usize> {
    if usable <= 4 {
        return Err(corrupt("a usable page size too small for an overflow page"));
    }
    Ok(usable.saturating_sub(4) as usize)
}

/// Reads a cell's whole payload, following its overflow chain if it has one.
///
/// `local` is the part already on the cell's own page; the caller holds that
/// page's pin, so the bytes stay valid while the chain is read.
pub fn read_payload(
    pager: &mut Pager,
    local: &[u8],
    total: u64,
    overflow: Option<PageId>,
    limits: &Limits,
) -> DbResult<Vec<u8>> {
    let mut payload = Vec::new();
    read_payload_into(pager, local, total, overflow, limits, &mut payload)?;
    Ok(payload)
}

/// Reads a payload into a buffer the caller keeps.
///
/// The buffer is cleared and refilled, so a caller that keeps one across rows
/// allocates once rather than once per row. That matters because reading a row
/// is what a scan does per row, and an allocation and a free were a fifth of
/// the cost of the whole step.
/// @param pager - the pager holding the overflow pages
/// @param local - the part of the payload that is on the cell's own page
/// @param total - how long the whole payload is
/// @param overflow - the first overflow page, when there is one
/// @param limits - the run-time limits
/// @param payload - the buffer to fill
pub fn read_payload_into(
    pager: &mut Pager,
    local: &[u8],
    total: u64,
    overflow: Option<PageId>,
    limits: &Limits,
    payload: &mut Vec<u8>,
) -> DbResult<()> {
    if !limits.permits_length(total) {
        return Err(too_big(format!(
            "a payload of {total} bytes exceeds the length limit of {}",
            limits.get(Limit::Length)
        )));
    }
    // The length limit alone is not a bound on what a *file* can hold: it is a
    // gigabyte by default, and a cell in a twenty-kilobyte database can claim
    // all of it in four bytes of varint. Sizing a buffer from that number
    // hands an attacker a gigabyte allocation per row. Nothing can be longer
    // than the database it came out of, so that is the bound the buffer uses.
    let possible = pager.largest_possible_payload();
    if total > possible {
        return Err(corrupt(format!(
            "a payload of {total} bytes in a database of {possible} bytes"
        )));
    }
    let total_usize = usize::try_from(total)
        .map_err(|_| too_big("a payload longer than this machine's memory"))?;
    if local.len() > total_usize {
        return Err(corrupt("a cell whose local payload exceeds its total"));
    }
    payload.clear();
    payload.reserve(total_usize);
    payload.extend_from_slice(local);
    if payload.len() == total_usize {
        if overflow.is_some() {
            return Err(corrupt("a complete payload that also has an overflow page"));
        }
        return Ok(());
    }
    let Some(head) = overflow else {
        return Err(corrupt(format!(
            "a payload of {total} bytes with only {} local and no overflow",
            local.len()
        )));
    };

    let per_page = payload_per_page(pager.usable_size()?)?;
    let mut visited: BTreeSet<u32> = BTreeSet::new();
    let mut next = Some(head);
    // A chain cannot be longer than the database, and one extra hop is enough
    // to distinguish "ends exactly here" from "keeps going".
    let hop_limit = u64::from(pager.page_count()).saturating_add(1);
    let mut hops = 0u64;

    while payload.len() < total_usize {
        let Some(page_id) = next else {
            return Err(corrupt(format!(
                "an overflow chain ended {} bytes short",
                total_usize.saturating_sub(payload.len())
            )));
        };
        hops = hops.saturating_add(1);
        if hops > hop_limit {
            return Err(corrupt("an overflow chain longer than the database"));
        }
        if !visited.insert(page_id.get()) {
            return Err(corrupt(format!(
                "an overflow chain that returns to page {}",
                page_id.get()
            )));
        }
        let pin = pager.get_page(page_id)?;
        let page = pin.bytes();
        let raw_next = bytes::read_u32(page, 0)?;
        next = if raw_next == 0 {
            None
        } else {
            Some(
                PageId::from_persisted(raw_next)
                    .map_err(|_| corrupt("an overflow chain pointing at page zero"))?,
            )
        };
        let wanted = total_usize.saturating_sub(payload.len()).min(per_page);
        let chunk = bytes::window(page, 4, wanted)?;
        payload.extend_from_slice(chunk);
    }

    if next.is_some() {
        return Err(corrupt(
            "an overflow chain that continues past the end of its payload",
        ));
    }
    Ok(())
}

/// Where a payload keeps one run of its bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PayloadSpan {
    /// The page the bytes are on.
    pub page: PageId,
    /// Their offset within it.
    pub offset: usize,
    /// How many there are.
    pub len: usize,
}

/// Where a payload lives, so a range of it can be found without reading it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PayloadPlace {
    /// The page the cell is on.
    pub page: PageId,
    /// Where the cell's own share of the payload starts on it.
    pub local_offset: usize,
    /// How many bytes that share is.
    pub local_len: usize,
    /// The payload's whole length.
    pub total: u64,
    /// The first page of the overflow chain, when there is one.
    pub overflow: Option<PageId>,
}

/// Returns the runs covering `start..start + len` of a payload.
///
/// This is what makes an incremental blob incremental: a hundred-megabyte
/// value has one byte changed by writing one page, because the range is mapped
/// onto pages rather than the value being read, patched and written back.
///
/// The chain is walked from its head, which costs one page read per overflow
/// page skipped. SQLite does the same, and it is why a blob written from front
/// to back is linear and one written back to front is quadratic.
pub fn locate(
    pager: &mut Pager,
    place: &PayloadPlace,
    start: u64,
    len: u64,
) -> DbResult<Vec<PayloadSpan>> {
    let end = start.saturating_add(len);
    if end > place.total {
        return Err(corrupt(format!(
            "bytes {start}..{end} of a payload of {}",
            place.total
        )));
    }
    let mut spans = Vec::new();
    if len == 0 {
        return Ok(spans);
    }
    let local_len = place.local_len as u64;
    if start < local_len {
        let taken = len.min(local_len.saturating_sub(start));
        spans.push(PayloadSpan {
            page: place.page,
            offset: place
                .local_offset
                .saturating_add(usize::try_from(start).unwrap_or(usize::MAX)),
            len: usize::try_from(taken).unwrap_or(0),
        });
        if taken == len {
            return Ok(spans);
        }
    }
    let per_page = payload_per_page(pager.usable_size()?)? as u64;
    if per_page == 0 {
        return Err(corrupt("an overflow page with no room for payload"));
    }
    let Some(head) = place.overflow else {
        return Err(corrupt(
            "a payload that needs an overflow chain and has none",
        ));
    };
    // Where in the chain the wanted range begins, and how far into that page.
    let after_local = start.max(local_len).saturating_sub(local_len);
    let mut page = head;
    let mut skipped = after_local / per_page;
    let mut offset_in_page = after_local % per_page;
    let hop_limit = u64::from(pager.page_count()).saturating_add(1);
    let mut hops = 0u64;
    while skipped > 0 {
        page = next_in_chain(pager, page)?
            .ok_or_else(|| corrupt("an overflow chain ended before its payload did"))?;
        skipped = skipped.saturating_sub(1);
        hops = hops.saturating_add(1);
        if hops > hop_limit {
            return Err(corrupt("an overflow chain that loops"));
        }
    }
    let mut remaining = end.saturating_sub(start.max(local_len));
    while remaining > 0 {
        let room = per_page.saturating_sub(offset_in_page);
        let taken = remaining.min(room);
        spans.push(PayloadSpan {
            page,
            offset: OVERFLOW_HEADER
                .saturating_add(usize::try_from(offset_in_page).unwrap_or(usize::MAX)),
            len: usize::try_from(taken).unwrap_or(0),
        });
        remaining = remaining.saturating_sub(taken);
        offset_in_page = 0;
        if remaining == 0 {
            break;
        }
        page = next_in_chain(pager, page)?
            .ok_or_else(|| corrupt("an overflow chain ended before its payload did"))?;
        hops = hops.saturating_add(1);
        if hops > hop_limit {
            return Err(corrupt("an overflow chain that loops"));
        }
    }
    Ok(spans)
}

/// How many bytes of an overflow page are the pointer to the next one.
pub const OVERFLOW_HEADER: usize = 4;

/// Returns the page after this one in an overflow chain.
fn next_in_chain(pager: &mut Pager, page: PageId) -> DbResult<Option<PageId>> {
    let pin = pager.get_page(page)?;
    let raw = bytes::read_u32(pin.bytes(), 0)?;
    drop(pin);
    if raw == 0 {
        return Ok(None);
    }
    Ok(Some(PageId::from_persisted(raw)?))
}

/// Counts the pages an overflow chain occupies, validating it as it goes.
///
/// The integrity check needs the page numbers rather than the bytes, and
/// reading the payload to count them would copy megabytes for nothing.
pub fn chain_pages(
    pager: &mut Pager,
    total: u64,
    local: usize,
    overflow: Option<PageId>,
) -> DbResult<Vec<PageId>> {
    let mut pages = Vec::new();
    let Some(head) = overflow else {
        return Ok(pages);
    };
    let possible = pager.largest_possible_payload();
    if total > possible {
        return Err(corrupt(format!(
            "a payload of {total} bytes in a database of {possible} bytes"
        )));
    }
    let per_page = payload_per_page(pager.usable_size()?)? as u64;
    let mut remaining = total.saturating_sub(local as u64);
    let mut visited: BTreeSet<u32> = BTreeSet::new();
    let mut next = Some(head);
    let hop_limit = u64::from(pager.page_count()).saturating_add(1);
    let mut hops = 0u64;

    while remaining > 0 {
        let Some(page_id) = next else {
            return Err(corrupt("an overflow chain ended before its payload did"));
        };
        hops = hops.saturating_add(1);
        if hops > hop_limit {
            return Err(corrupt("an overflow chain longer than the database"));
        }
        if !visited.insert(page_id.get()) {
            return Err(corrupt(format!(
                "an overflow chain that returns to page {}",
                page_id.get()
            )));
        }
        pages.push(page_id);
        let pin = pager.get_page(page_id)?;
        let raw_next = bytes::read_u32(pin.bytes(), 0)?;
        next = if raw_next == 0 {
            None
        } else {
            Some(
                PageId::from_persisted(raw_next)
                    .map_err(|_| corrupt("an overflow chain pointing at page zero"))?,
            )
        };
        remaining = remaining.saturating_sub(per_page);
    }
    if next.is_some() {
        return Err(corrupt(
            "an overflow chain that continues past the end of its payload",
        ));
    }
    Ok(pages)
}

/// Writes the part of a payload that did not fit on its page into a fresh
/// chain, and returns the first page.
///
/// The pages are allocated before any of them is written, because each one has
/// to hold the number of the next and a chain built forwards would need a
/// second pass anyway. Allocation is the step that can fail - the disk is full,
/// the freelist is corrupt - and doing it first means a failure leaves pages on
/// the freelist rather than a half-linked chain.
///
/// The head's pointer-map entry names no parent yet. Which B-tree page owns the
/// cell is not known until the cell has been placed, and a balance can move it
/// again; [`crate::ptrmap::refresh_btree_page`] writes the real parent once the
/// page holding the cell is final.
pub fn write_chain(pager: &mut Pager, tail: &[u8]) -> DbResult<Option<PageId>> {
    if tail.is_empty() {
        return Ok(None);
    }
    let per_page = payload_per_page(pager.usable_size()?)?;
    let count = tail.len().div_ceil(per_page);
    let mut pages = Vec::with_capacity(count);
    for _ in 0..count {
        pages.push(alloc::allocate_page(pager)?);
    }
    for (index, page) in pages.iter().copied().enumerate() {
        let next = pages.get(index.saturating_add(1)).copied();
        let start = index.saturating_mul(per_page);
        let end = start.saturating_add(per_page).min(tail.len());
        let chunk = tail
            .get(start..end)
            .ok_or_else(|| corrupt("an overflow chunk outside its payload"))?
            .to_vec();
        pager.edit_page(page, |raw| {
            bytes::write_u32(raw, 0, next.map(PageId::get).unwrap_or(0))?;
            let target = bytes::window_mut(raw, 4, chunk.len())?;
            target.copy_from_slice(&chunk);
            Ok(())
        })?;
        match index.checked_sub(1).and_then(|before| pages.get(before)) {
            Some(previous) => {
                ptrmap::put(pager, page, ptrmap::Entry::overflow_next(*previous))?;
            }
            None => {
                ptrmap::put(
                    pager,
                    page,
                    ptrmap::Entry {
                        kind: ptrmap::OVERFLOW1,
                        parent: 0,
                    },
                )?;
            }
        }
    }
    Ok(pages.first().copied())
}

/// Returns every page of a chain to the freelist.
///
/// The chain is walked and validated first, so a corrupt chain is refused
/// before a single page has been freed. Freeing as it walked would put half a
/// chain on the freelist and leave the other half owned by nothing.
pub fn free_chain(
    pager: &mut Pager,
    total: u64,
    local: usize,
    head: Option<PageId>,
) -> DbResult<()> {
    let pages = chain_pages(pager, total, local, head)?;
    for page in pages {
        alloc::free_page(pager, page)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload area of an overflow page is the usable size less the
    /// four-byte pointer, and a page too small to hold one is refused.
    #[test]
    fn an_overflow_page_holds_the_usable_size_less_its_pointer() {
        assert_eq!(payload_per_page(4096).unwrap(), 4092);
        assert_eq!(payload_per_page(512).unwrap(), 508);
        assert!(payload_per_page(4).is_err());
        assert!(payload_per_page(0).is_err());
    }
}
