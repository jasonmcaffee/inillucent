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

use rustdb_base::bytes;
use rustdb_base::error::{corrupt, too_big};
use rustdb_base::ids::PageId;
use rustdb_base::limits::{Limit, Limits};
use rustdb_base::DbResult;

use crate::pager::Pager;

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
    let mut payload = Vec::with_capacity(total_usize);
    payload.extend_from_slice(local);
    if payload.len() == total_usize {
        if overflow.is_some() {
            return Err(corrupt("a complete payload that also has an overflow page"));
        }
        return Ok(payload);
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
    Ok(payload)
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
