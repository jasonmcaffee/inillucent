//! The three-layer storage model a crash is simulated against.
//!
//! Invariant: a byte is durable only after a sync that returned success. Every
//! other written byte is in a layer that a power loss is allowed to discard,
//! and the crash model really does discard it.
//!
//! Real storage has three places a write can be sitting when the power goes:
//! the process's own buffers, the operating system's cache, and the media. The
//! VFS writes straight through its own buffers, so two layers are modelled
//! here: `cached`, holding sectors written but not yet synced, and `durable`,
//! holding what has actually landed. A crash resolves each cached sector
//! independently - applied, dropped, or torn - which is what makes a crash test
//! find the ordering bugs that a "lose everything unsynced" model misses.

use inillucent_base::rng::Rng;

/// What a device promises about writes that a power loss interrupts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaModel {
    /// The unit a write is torn at.
    pub sector_size: u32,
    /// A power loss cannot damage bytes the write was not touching.
    pub powersafe_overwrite: bool,
    /// Writes reach the media in the order they were issued.
    pub sequential: bool,
    /// The largest write the device performs atomically.
    pub atomic_write_size: u32,
    /// Whether `sync` returns success without making anything durable.
    ///
    /// **A drive that acknowledges a cache flush it did not perform**
    /// (task-2066 section 4.4.8). Consumer drives have shipped with write
    /// caching that ignores the flush, and a container file system can sit on
    /// one. Every durability argument in this engine rests on `sync` meaning
    /// what it says, so the interesting question is not whether a lying sync
    /// loses data - it does - but whether the loss is *detected* on the next
    /// open rather than served as an answer. That is what a campaign with this
    /// on measures.
    pub sync_is_a_lie: bool,
}

impl Default for MediaModel {
    /// The pessimistic model: 512-byte sectors, no ordering, no atomicity, and
    /// a power loss that can damage a sector it was part-way through.
    fn default() -> MediaModel {
        MediaModel {
            sector_size: 512,
            powersafe_overwrite: false,
            sequential: false,
            atomic_write_size: 0,
            // Off by default: a lying sync is a broken device rather than a
            // pessimistic one, and every campaign that does not ask for it is
            // asking what happens on a device that works.
            sync_is_a_lie: false,
        }
    }
}

/// What happens to one unsynced sector when the power goes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SectorOutcome {
    /// The write landed in full.
    Applied,
    /// The write never reached the media.
    Dropped,
    /// The sector holds a mixture of its old and new contents.
    Torn,
    /// The sector holds neither; the power went out mid-write and the device
    /// does not promise powersafe overwrite.
    Garbage,
}

/// One simulated file: what is durable, and what is only cached.
#[derive(Clone, Debug, Default)]
pub struct SimFileImage {
    /// The bytes a crash right now would leave behind, before cached sectors
    /// are resolved.
    durable: Vec<u8>,
    /// Sector index to its pending contents, for writes that have not synced.
    cached: std::collections::BTreeMap<u64, Vec<u8>>,
    /// The length the file will have once its cached state is resolved.
    visible_len: u64,
    /// The smallest length the file has been truncated to since the last sync.
    ///
    /// Truncation is not durable until a sync, so the old bytes are still in
    /// `durable`; this remembers that everything from here on now reads as
    /// zero, which is what makes shrinking and then growing produce zeroes
    /// rather than resurrecting the discarded tail.
    truncated_to: u64,
    /// Whether the length change itself has been synced.
    len_durable: bool,
}

impl SimFileImage {
    /// Creates an empty file.
    pub fn new() -> SimFileImage {
        SimFileImage {
            truncated_to: u64::MAX,
            ..SimFileImage::default()
        }
    }

    /// Returns the length a reader sees now.
    pub fn len(&self) -> u64 {
        self.visible_len
    }

    /// Reports whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.visible_len == 0
    }

    /// Reads what a reader sees now: durable bytes overlaid with cached ones.
    ///
    /// Returns how many bytes were readable; the caller decides whether a
    /// shorter answer is a short read.
    pub fn read(&self, model: MediaModel, offset: u64, output: &mut [u8]) -> usize {
        let available = self.visible_len.saturating_sub(offset);
        let readable = available.min(output.len() as u64) as usize;
        for index in 0..readable {
            let position = offset.saturating_add(index as u64);
            let byte = self.byte_at(model, position);
            if let Some(slot) = output.get_mut(index) {
                *slot = byte;
            }
        }
        for slot in output.iter_mut().skip(readable) {
            *slot = 0;
        }
        readable
    }

    /// Returns the byte a reader sees at `position`.
    fn byte_at(&self, model: MediaModel, position: u64) -> u8 {
        let sector = position / u64::from(model.sector_size);
        let within = (position % u64::from(model.sector_size)) as usize;
        if let Some(pending) = self.cached.get(&sector) {
            if let Some(byte) = pending.get(within) {
                return *byte;
            }
        }
        if position >= self.truncated_to {
            return 0;
        }
        self.durable.get(position as usize).copied().unwrap_or(0)
    }

    /// Writes `input` at `offset` into the cached layer.
    pub fn write(&mut self, model: MediaModel, offset: u64, input: &[u8]) {
        let end = offset.saturating_add(input.len() as u64);
        if end > self.visible_len {
            self.visible_len = end;
            self.len_durable = false;
        }
        let sector_size = u64::from(model.sector_size);
        for (index, byte) in input.iter().enumerate() {
            let position = offset.saturating_add(index as u64);
            let sector = position / sector_size;
            let within = (position % sector_size) as usize;
            if !self.cached.contains_key(&sector) {
                let existing = self.durable_sector(model, sector);
                self.cached.insert(sector, existing);
            }
            if let Some(slot) = self
                .cached
                .get_mut(&sector)
                .and_then(|bytes| bytes.get_mut(within))
            {
                *slot = *byte;
            }
        }
    }

    /// Returns the durable contents of one sector, zero-filled past the end.
    fn durable_sector(&self, model: MediaModel, sector: u64) -> Vec<u8> {
        let size = model.sector_size as usize;
        let start = (sector * u64::from(model.sector_size)) as usize;
        let mut bytes = vec![0u8; size];
        for (index, slot) in bytes.iter_mut().enumerate() {
            if let Some(byte) = self.durable.get(start.saturating_add(index)) {
                *slot = *byte;
            }
        }
        bytes
    }

    /// Sets the file's length, discarding or zero-extending as needed.
    pub fn truncate(&mut self, model: MediaModel, size: u64) {
        self.visible_len = size;
        self.len_durable = false;
        self.truncated_to = self.truncated_to.min(size);
        let sector_size = u64::from(model.sector_size);
        let last_sector = size.div_ceil(sector_size);
        self.cached.retain(|sector, _| *sector < last_sector);
        let boundary = size / sector_size;
        let within = (size % sector_size) as usize;
        if within != 0 {
            if let Some(bytes) = self.cached.get_mut(&boundary) {
                for slot in bytes.iter_mut().skip(within) {
                    *slot = 0;
                }
            }
        }
    }

    /// Makes every cached byte durable, as a successful sync does.
    pub fn sync(&mut self, model: MediaModel) {
        // **A lying sync leaves the cache exactly as it was.** See
        // `MediaModel::sync_is_a_lie`: the call returns, the caller believes
        // the bytes are durable, and a power loss resolves the cached sectors
        // the way an unsynced write is resolved - dropped, torn or garbage.
        if model.sync_is_a_lie {
            return;
        }
        if self.truncated_to < self.durable.len() as u64 {
            self.durable.truncate(self.truncated_to as usize);
        }
        let sectors: Vec<u64> = self.cached.keys().copied().collect();
        for sector in sectors {
            if let Some(bytes) = self.cached.remove(&sector) {
                self.place_sector(model, sector, &bytes);
            }
        }
        self.durable.resize(self.visible_len as usize, 0);
        self.truncated_to = u64::MAX;
        self.len_durable = true;
    }

    /// Writes one resolved sector into the durable image.
    fn place_sector(&mut self, model: MediaModel, sector: u64, bytes: &[u8]) {
        let start = (sector * u64::from(model.sector_size)) as usize;
        let end = start.saturating_add(bytes.len());
        if self.durable.len() < end {
            self.durable.resize(end, 0);
        }
        for (index, byte) in bytes.iter().enumerate() {
            if let Some(slot) = self.durable.get_mut(start.saturating_add(index)) {
                *slot = *byte;
            }
        }
    }

    /// Resolves every cached sector as a power loss would and returns the
    /// image that a reopen would find, together with what happened to each
    /// sector so a failing run can say which one was torn.
    pub fn crash(
        &self,
        model: MediaModel,
        rng: &mut Rng,
    ) -> (SimFileImage, Vec<(u64, SectorOutcome)>) {
        // A truncation is no more durable than a write: until it is synced,
        // the media still holds the old length, so recovery starts from the
        // durable image rather than from what the process could see.
        let mut recovered = SimFileImage {
            durable: self.durable.clone(),
            cached: std::collections::BTreeMap::new(),
            visible_len: self.durable.len() as u64,
            truncated_to: u64::MAX,
            len_durable: true,
        };
        let mut outcomes = Vec::new();
        for (sector, bytes) in &self.cached {
            let outcome = choose_outcome(model, bytes.len() as u32, rng);
            apply_outcome(&mut recovered, model, *sector, bytes, outcome, rng);
            outcomes.push((*sector, outcome));
        }
        recovered.visible_len = recovered.durable.len() as u64;
        (recovered, outcomes)
    }

    /// Returns the durable bytes, for a caller that wants to hash the image.
    pub fn durable_bytes(&self) -> &[u8] {
        &self.durable
    }
}

/// Chooses what a power loss does to one unsynced sector.
fn choose_outcome(model: MediaModel, width: u32, rng: &mut Rng) -> SectorOutcome {
    if model.atomic_write_size >= width && model.atomic_write_size != 0 {
        return if rng.chance(1, 2) {
            SectorOutcome::Applied
        } else {
            SectorOutcome::Dropped
        };
    }
    match rng.below(if model.powersafe_overwrite { 3 } else { 4 }) {
        0 => SectorOutcome::Applied,
        1 => SectorOutcome::Dropped,
        2 => SectorOutcome::Torn,
        _ => SectorOutcome::Garbage,
    }
}

/// Applies one sector outcome to the recovered image.
fn apply_outcome(
    recovered: &mut SimFileImage,
    model: MediaModel,
    sector: u64,
    bytes: &[u8],
    outcome: SectorOutcome,
    rng: &mut Rng,
) {
    match outcome {
        SectorOutcome::Dropped => {}
        SectorOutcome::Applied => recovered.place_sector(model, sector, bytes),
        SectorOutcome::Torn => {
            let cut = rng.below(bytes.len() as u64) as usize;
            let old = recovered.durable_sector(model, sector);
            let mut mixed = bytes.to_vec();
            for index in cut..mixed.len() {
                if let (Some(slot), Some(byte)) = (mixed.get_mut(index), old.get(index)) {
                    *slot = *byte;
                }
            }
            recovered.place_sector(model, sector, &mixed);
        }
        SectorOutcome::Garbage => {
            let mut noise = vec![0u8; bytes.len()];
            rng.fill(&mut noise);
            recovered.place_sector(model, sector, &noise);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synced write survives a crash unchanged. This is the one promise the
    /// whole durability program rests on, so it is checked over many seeds.
    #[test]
    fn a_synced_write_always_survives() {
        let model = MediaModel::default();
        for seed in 0..500 {
            let mut image = SimFileImage::new();
            image.write(model, 0, &[0x11; 1024]);
            image.sync(model);
            let mut rng = Rng::new(seed);
            let (recovered, outcomes) = image.crash(model, &mut rng);
            assert!(outcomes.is_empty(), "a synced image had pending sectors");
            let mut buffer = [0u8; 1024];
            assert_eq!(recovered.read(model, 0, &mut buffer), 1024);
            assert_eq!(buffer, [0x11; 1024]);
        }
    }

    /// An unsynced write must sometimes be lost, or a crash test would prove
    /// nothing about durability.
    #[test]
    fn an_unsynced_write_is_sometimes_lost() {
        let model = MediaModel::default();
        let mut lost = 0;
        let mut kept = 0;
        for seed in 0..200 {
            let mut image = SimFileImage::new();
            image.write(model, 0, &[0x22; 512]);
            image.sync(model);
            image.write(model, 0, &[0x33; 512]);
            let mut rng = Rng::new(seed);
            let (recovered, _) = image.crash(model, &mut rng);
            let mut buffer = [0u8; 512];
            recovered.read(model, 0, &mut buffer);
            if buffer == [0x33; 512] {
                kept += 1;
            } else if buffer == [0x22; 512] {
                lost += 1;
            }
        }
        assert!(lost > 0, "an unsynced write was never lost");
        assert!(kept > 0, "an unsynced write was never kept");
    }

    /// A torn sector must be a mixture of the old and new bytes, never a
    /// mixture with bytes that were in neither.
    #[test]
    fn a_torn_sector_mixes_only_old_and_new() {
        let model = MediaModel {
            powersafe_overwrite: true,
            ..MediaModel::default()
        };
        let mut saw_tear = false;
        for seed in 0..400 {
            let mut image = SimFileImage::new();
            image.write(model, 0, &[0xaa; 512]);
            image.sync(model);
            image.write(model, 0, &[0xbb; 512]);
            let mut rng = Rng::new(seed);
            let (recovered, outcomes) = image.crash(model, &mut rng);
            let mut buffer = [0u8; 512];
            recovered.read(model, 0, &mut buffer);
            assert!(
                buffer.iter().all(|byte| *byte == 0xaa || *byte == 0xbb),
                "seed {seed} produced a byte that was in neither version"
            );
            if outcomes
                .iter()
                .any(|(_, outcome)| *outcome == SectorOutcome::Torn)
            {
                saw_tear = true;
            }
        }
        assert!(saw_tear, "no seed produced a torn sector");
    }

    /// A powersafe device must never produce bytes that were in neither
    /// version, and a non-powersafe one must sometimes do exactly that.
    #[test]
    fn powersafe_overwrite_changes_what_a_crash_can_produce() {
        let unsafe_model = MediaModel::default();
        let mut saw_garbage = false;
        for seed in 0..400 {
            let mut image = SimFileImage::new();
            image.write(unsafe_model, 0, &[0xaa; 512]);
            image.sync(unsafe_model);
            image.write(unsafe_model, 0, &[0xbb; 512]);
            let mut rng = Rng::new(seed);
            let (_, outcomes) = image.crash(unsafe_model, &mut rng);
            if outcomes
                .iter()
                .any(|(_, outcome)| *outcome == SectorOutcome::Garbage)
            {
                saw_garbage = true;
            }
        }
        assert!(saw_garbage, "a non-powersafe device never damaged a sector");
    }

    /// Truncation must drop the cached sectors past the new end, or a crash
    /// could resurrect bytes the file no longer has.
    #[test]
    fn truncation_drops_cached_sectors_past_the_end() {
        let model = MediaModel::default();
        let mut image = SimFileImage::new();
        image.write(model, 0, &[0x44; 2048]);
        image.truncate(model, 512);
        let mut rng = Rng::new(1);
        let (recovered, _) = image.crash(model, &mut rng);
        assert!(
            recovered.len() <= 512,
            "the file grew back to {}",
            recovered.len()
        );
    }
}
