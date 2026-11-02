//! Incoming H.264 video: RFC 6184 depacketizing per remote SSRC and software decoding on a
//! dedicated thread. Frames stay bounded (1080p RGBA) and no video is ever written to disk.
//! Discord's video signaling is unofficial; live interoperability is unverified.
use std::{
	collections::HashMap,
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, AtomicU64, Ordering},
		mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
	},
	time::{Duration, Instant},
};

/// Users whose decoder hit undecodable data; the transport turns these into keyframe requests.
pub(crate) type Lost = Arc<Mutex<Vec<u64>>>;

/// Largest reassembled (still DAVE-encrypted) access unit accepted from one remote sender.
/// Fork: 1440p/4K keyframes exceed the upstream 2 MiB, which dropped every picture.
pub const MAX_FRAME_BYTES: usize = platform::video::MAX_ACCESS_UNIT;
/// Remote video senders tracked per transport; Discord forwards at most a few at once.
pub const MAX_SOURCES: usize = 16;
/// Decoders kept alive at once; each holds reference pictures for one remote user.
const MAX_DECODERS: usize = 8;
const QUEUE_BYTES: usize = 64 * 1024 * 1024;
/// Fork: live streams up to 4K/ultrawide (upstream stopped at 1080p, so 1440p and 4K
/// streams showed "waiting for video" forever while their audio played).
const MAX_WIDTH: u32 = platform::video::MAX_LIVE_SIDE;
const MAX_PIXELS: u64 = platform::video::MAX_LIVE_PIXELS;
const START_CODE: [u8; 4] = [0, 0, 0, 1];
// Fork: 300 ms (was 150) so a briefly slow decoder does not drop pictures and wait for a keyframe.
const MAX_DECODE_AGE: Duration = Duration::from_millis(300);

/// One decoded remote picture in the decoder's native layout (NV12 or I420 on every
/// current backend); the renderer converts colour.
pub struct RemoteFrame<'a> {
	pub user: u64,
	pub picture: platform::video::Picture<&'a [u8]>,
}
pub type VideoSink = Arc<dyn Fn(RemoteFrame<'_>) + Send + Sync>;

pub(crate) struct DecoderQueue {
	send: SyncSender<Decode>,
	#[cfg(test)]
	worker: Option<(std::thread::JoinHandle<()>, Receiver<()>)>,
	bytes: Arc<tokio::sync::Semaphore>,
	// One cancellable lifetime per user; queued frames keep the old lifetime on restart.
	// This table has at most MAX_SOURCES entries. Old tokens survive only in the
	// bounded frame queue/worker and decoder callbacks, never an accumulating tombstone list.
	active: Mutex<HashMap<u64, Arc<AtomicBool>>>,
	/// Decoded pictures delivered to the sink and decoder failures, for diagnostics only.
	pub counters: Arc<DecoderCounters>,
}

#[cfg(test)]
impl Drop for DecoderQueue {
	fn drop(&mut self) {
		let Some((worker, completed)) = self.worker.take() else {
			return;
		};
		// Close the queue before waiting: the native decoder is owned by this worker.
		drop(std::mem::replace(&mut self.send, sync_channel(0).0));
		assert!(
			!matches!(
				completed.recv_timeout(Duration::from_secs(5)),
				Err(std::sync::mpsc::RecvTimeoutError::Timeout)
			),
			"Video decoder did not terminate after its queue closed"
		);
		worker.join().expect("Video decoder worker panicked");
	}
}

#[derive(Default)]
pub(crate) struct DecoderCounters {
	pub pictures: AtomicU64,
	pub errors: AtomicU64,
	pub hardware: AtomicU64,
	pub software: AtomicU64,
	pub queue_ms: AtomicU64,
	pub stale: AtomicU64,
}

/// A cleartext Annex-B access unit handed to the decoder thread.
pub(crate) struct Encoded {
	pub user: u64,
	pub data: Vec<u8>,
	pub keyframe: bool,
}

enum Decode {
	Frame(
		Encoded,
		tokio::sync::OwnedSemaphorePermit,
		Instant,
		Arc<AtomicBool>,
	),
	Wake,
	#[cfg(test)]
	Barrier(SyncSender<usize>),
}

/// True when the cleartext Annex-B access unit carries both an SPS and a PPS, so a freshly
/// created decoder can start from it.
pub(crate) fn has_parameter_sets(frame: &[u8]) -> bool {
	let (mut sps, mut pps) = (false, false);
	let mut at = 0;
	while at + 4 <= frame.len() {
		let size = if frame[at..].starts_with(&[0, 0, 0, 1]) {
			4
		} else if frame[at..].starts_with(&[0, 0, 1]) {
			3
		} else {
			at += 1;
			continue;
		};
		match frame.get(at + size).map(|nal| nal & 0x1f) {
			Some(7) => sps = true,
			Some(8) => pps = true,
			_ => {}
		}
		at += size;
	}
	sps && pps
}

/// True when the cleartext Annex-B access unit carries an IDR slice.
pub(crate) fn is_keyframe(frame: &[u8]) -> bool {
	let mut at = 0;
	while at + 4 <= frame.len() {
		let size = if frame[at..].starts_with(&[0, 0, 0, 1]) {
			4
		} else if frame[at..].starts_with(&[0, 0, 1]) {
			3
		} else {
			at += 1;
			continue;
		};
		if frame.get(at + size).is_some_and(|nal| nal & 0x1f == 5) {
			return true;
		}
		at += size;
	}
	false
}

/// Reassembles RTP payloads of one SSRC into Annex-B access units (single NAL, STAP-A, FU-A).
#[derive(Default)]
pub(crate) struct Assembler {
	timestamp: u32,
	next_sequence: Option<u16>,
	frame: Vec<u8>,
	fragmenting: bool,
	broken: bool,
	started: bool,
	// Preserve loss across frame resets until Receivers schedules recovery.
	lost: bool,
	// Only packets of the current picture; the next timestamp expires missing fragments.
	pending: Vec<(u16, bool, Vec<u8>)>,
	pending_bytes: usize,
	/// Fork: holds packets after a gap so a retransmission can fill it.
	reorder: Reorder,
}

/// How long packets after a gap wait for the missing ones before the picture is given up.
/// A few round trips to Discord's media server; long enough for a NACK retransmission.
const REORDER_WAIT: Duration = Duration::from_millis(160);
/// Packets held behind one gap (about a quarter second of a 30 Mbit/s share).
const REORDER_PACKETS: usize = 1024;
const REORDER_BYTES: usize = 4 * 1024 * 1024;
/// A missing packet is requested again at most this often, a bounded number of times.
const NACK_RETRY: Duration = Duration::from_millis(40);
const NACK_ATTEMPTS: u8 = 3;

/// Fork: per-source sequence buffer in front of the assembler. Upstream gave up on a picture
/// as soon as one packet was missing and only asked for a new keyframe (at most every
/// 500 ms), so a single lost packet froze the stream for most of a second. Packets are now
/// released in order; a gap is held for `REORDER_WAIT` while its packets are NACKed, and
/// only then skipped (the assembler sees the gap and the keyframe path takes over).
#[derive(Default)]
struct Reorder {
	next: Option<u16>,
	/// `slots[i]` is sequence `next + i`.
	slots: std::collections::VecDeque<Option<(u32, bool, Vec<u8>)>>,
	bytes: usize,
	gap_since: Option<Instant>,
	/// Missing sequences already requested: (sequence, last request, attempts).
	requested: Vec<(u16, Instant, u8)>,
}
impl Reorder {
	/// Insert one packet; returns the packets now releasable in sequence order.
	fn insert(
		&mut self,
		sequence: u16,
		timestamp: u32,
		marker: bool,
		payload: &[u8],
		now: Instant,
	) -> Vec<(u16, u32, bool, Vec<u8>)> {
		let next = *self.next.get_or_insert(sequence);
		let distance = sequence.wrapping_sub(next) as usize;
		if distance >= 32768 {
			return Vec::new(); // Duplicate or a retransmission that arrived too late.
		}
		if distance >= REORDER_PACKETS || self.bytes + payload.len() > REORDER_BYTES {
			// Far ahead of a gap we cannot fill: release what we have and resume here.
			let mut out = self.skip_all();
			self.next = Some(sequence);
			self.place(0, timestamp, marker, payload);
			out.extend(self.drain());
			return out;
		}
		self.place(distance, timestamp, marker, payload);
		let mut out = self.drain();
		if self.slots.is_empty() {
			self.gap_since = None;
		} else {
			let since = *self.gap_since.get_or_insert(now);
			if now.duration_since(since) >= REORDER_WAIT {
				out.extend(self.skip_gap());
				self.gap_since = (!self.slots.is_empty()).then_some(now);
			}
		}
		out
	}
	fn place(&mut self, distance: usize, timestamp: u32, marker: bool, payload: &[u8]) {
		while self.slots.len() <= distance {
			self.slots.push_back(None);
		}
		if self.slots[distance].is_none() {
			self.bytes += payload.len();
			self.slots[distance] = Some((timestamp, marker, payload.to_vec()));
		}
	}
	/// Release the in-order run at the front.
	fn drain(&mut self) -> Vec<(u16, u32, bool, Vec<u8>)> {
		let mut out = Vec::new();
		while let Some(Some(_)) = self.slots.front() {
			let (timestamp, marker, payload) = self.slots.pop_front().flatten().expect("front");
			self.bytes -= payload.len();
			let sequence = self.next.expect("started");
			self.next = Some(sequence.wrapping_add(1));
			out.push((sequence, timestamp, marker, payload));
		}
		let next = self.next;
		self.requested
			.retain(|(sequence, ..)| next.is_some_and(|next| sequence.wrapping_sub(next) < 32768));
		out
	}
	/// Give up on the oldest gap: skip its missing packets and release what follows.
	fn skip_gap(&mut self) -> Vec<(u16, u32, bool, Vec<u8>)> {
		while let Some(None) = self.slots.front() {
			self.slots.pop_front();
			self.next = self.next.map(|next| next.wrapping_add(1));
		}
		self.drain()
	}
	fn skip_all(&mut self) -> Vec<(u16, u32, bool, Vec<u8>)> {
		let mut out = Vec::new();
		while !self.slots.is_empty() {
			out.extend(self.skip_gap());
		}
		self.gap_since = None;
		self.requested.clear();
		out
	}
	/// Missing sequences due for a (re)request.
	fn nacks(&mut self, now: Instant) -> Vec<u16> {
		let Some(next) = self.next else {
			return Vec::new();
		};
		let Some(last) = self.slots.iter().rposition(Option::is_some) else {
			return Vec::new();
		};
		let mut due = Vec::new();
		for (index, slot) in self.slots.iter().enumerate().take(last) {
			if slot.is_some() || due.len() >= 128 {
				continue;
			}
			let sequence = next.wrapping_add(index as u16);
			let full = self.requested.len() >= 512;
			match self.requested.iter_mut().find(|(seq, ..)| *seq == sequence) {
				Some((_, at, attempts)) => {
					if *attempts < NACK_ATTEMPTS && now.duration_since(*at) >= NACK_RETRY {
						*at = now;
						*attempts += 1;
						due.push(sequence);
					}
				}
				None if !full => {
					self.requested.push((sequence, now, 1));
					due.push(sequence);
				}
				None => {}
			}
		}
		due
	}
}

/// RFC 4585 Generic NACK for lost RTP sequences of one media SSRC.
pub(crate) fn nack(sender: u32, media: u32, lost: &[u16]) -> Option<([u8; 8], Vec<u8>)> {
	let mut entries: Vec<(u16, u16)> = Vec::new();
	for &sequence in lost {
		if let Some((pid, mask)) = entries.last_mut() {
			let offset = sequence.wrapping_sub(*pid);
			if (1..=16).contains(&offset) {
				*mask |= 1 << (offset - 1);
				continue;
			}
		}
		entries.push((sequence, 0));
	}
	if entries.is_empty() || entries.len() > 64 {
		return None;
	}
	let length = 2 + entries.len() as u16;
	let mut header = [0x81, 205, 0, 0, 0, 0, 0, 0];
	header[2..4].copy_from_slice(&length.to_be_bytes());
	header[4..].copy_from_slice(&sender.to_be_bytes());
	let mut body = Vec::with_capacity(4 + entries.len() * 4);
	body.extend_from_slice(&media.to_be_bytes());
	for (pid, mask) in entries {
		body.extend_from_slice(&pid.to_be_bytes());
		body.extend_from_slice(&mask.to_be_bytes());
	}
	Some((header, body))
}
impl Assembler {
	/// Feed one packet; returns a complete access unit when the marker closes an intact frame.
	pub fn push(
		&mut self,
		sequence: u16,
		timestamp: u32,
		marker: bool,
		payload: &[u8],
	) -> Option<Vec<u8>> {
		if let Some(expected) = self.next_sequence {
			let distance = sequence.wrapping_sub(expected);
			if distance >= 32768 {
				return None;
			} // Duplicate or late retransmission.
			if distance != 0 && self.started && timestamp == self.timestamp {
				if self.pending.iter().any(|(seq, _, _)| *seq == sequence) {
					return None;
				}
				if distance < 128
					&& self.pending.len() < 128
					&& payload.len() <= 4096
					&& self.pending_bytes + payload.len() <= 256 * 1024
				{
					self.pending_bytes += payload.len();
					self.pending.push((sequence, marker, payload.to_vec()));
					return None;
				}
				self.pending.clear();
				self.pending_bytes = 0;
			}
		}
		if timestamp != self.timestamp {
			self.pending.clear();
			self.pending_bytes = 0;
		}
		let mut complete = self.push_ordered(sequence, timestamp, marker, payload);
		while complete.is_none() {
			let Some(index) = self
				.pending
				.iter()
				.position(|(seq, _, _)| Some(*seq) == self.next_sequence)
			else {
				break;
			};
			let (seq, marker, payload) = self.pending.swap_remove(index);
			self.pending_bytes -= payload.len();
			complete = self.push_ordered(seq, timestamp, marker, &payload);
		}
		complete
	}
	fn push_ordered(
		&mut self,
		sequence: u16,
		timestamp: u32,
		marker: bool,
		payload: &[u8],
	) -> Option<Vec<u8>> {
		if self.started && timestamp != self.timestamp {
			// A new picture started before the previous marker arrived: drop the partial one.
			self.lost = true;
			self.reset_frame();
			self.timestamp = timestamp;
		}
		if !self.started {
			self.started = true;
			self.timestamp = timestamp;
		}
		if self
			.next_sequence
			.is_some_and(|expected| expected != sequence)
		{
			self.broken = true;
		}
		self.next_sequence = Some(sequence.wrapping_add(1));
		if !self.broken && self.append(payload).is_err() {
			self.broken = true;
		}
		self.lost |= self.broken;
		if !marker {
			return None;
		}
		let complete = (!self.broken && !self.fragmenting && !self.frame.is_empty())
			.then(|| std::mem::take(&mut self.frame));
		self.lost |= complete.is_none();
		self.reset_frame();
		self.started = false;
		complete
	}
	fn reset_frame(&mut self) {
		self.frame.clear();
		if self.frame.capacity() > 256 * 1024 {
			self.frame = Vec::new();
		}
		self.fragmenting = false;
		self.broken = false;
	}
	fn extend(&mut self, parts: &[&[u8]]) -> Result<(), ()> {
		let total: usize = parts.iter().map(|part| part.len()).sum();
		if self.frame.len() + total > MAX_FRAME_BYTES {
			return Err(());
		}
		if self.frame.len() + total > self.frame.capacity()
			&& self.frame.capacity().saturating_mul(2) > MAX_FRAME_BYTES
		{
			self.frame.reserve_exact(MAX_FRAME_BYTES - self.frame.len());
		}
		for part in parts {
			self.frame.extend_from_slice(part);
		}
		Ok(())
	}
	fn append(&mut self, payload: &[u8]) -> Result<(), ()> {
		let Some(&indicator) = payload.first() else {
			return Err(());
		};
		match indicator & 0x1f {
			1..=23 => {
				if self.fragmenting {
					return Err(());
				}
				self.extend(&[&START_CODE, payload])
			}
			24 => {
				if self.fragmenting {
					return Err(());
				}
				let mut at = 1;
				while at < payload.len() {
					if at + 2 > payload.len() {
						return Err(());
					}
					let size = usize::from(u16::from_be_bytes([payload[at], payload[at + 1]]));
					at += 2;
					if size == 0 || at + size > payload.len() {
						return Err(());
					}
					self.extend(&[&START_CODE, &payload[at..at + size]])?;
					at += size;
				}
				Ok(())
			}
			28 => {
				if payload.len() < 2 {
					return Err(());
				}
				let header = payload[1];
				let start = header & 0x80 != 0;
				let end = header & 0x40 != 0;
				if start == self.fragmenting {
					return Err(());
				}
				if start {
					let nal_header = (indicator & 0xe0) | (header & 0x1f);
					self.extend(&[&START_CODE, &[nal_header], &payload[2..]])?;
				} else {
					self.extend(&[&payload[2..]])?;
				}
				self.fragmenting = !end;
				Ok(())
			}
			_ => Err(()),
		}
	}
}

/// Remote video SSRC ownership and per-source reassembly for one media transport.
#[derive(Default)]
pub(crate) struct Receivers {
	sources: Vec<(u32, u64, Assembler)>,
	rtx: Vec<(u32, u32)>,
	/// Users whose decoder lost a reference picture; only a keyframe restarts their video.
	awaiting_keyframe: Vec<u64>,
	/// Depacketizer outcomes since the last `take_stats`, for diagnostics.
	stats: ReceiveStats,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ReceiveStats {
	pub unknown_ssrc: u64,
	pub incomplete: u64,
	pub complete: u64,
}
impl Receivers {
	/// Bind an announced video SSRC to a user; zero clears that user's sources.
	pub fn announce(&mut self, user: u64, ssrc: u32) -> Result<(), &'static str> {
		if ssrc == 0 {
			self.remove(user);
			return Ok(());
		}
		if let Some(entry) = self.sources.iter_mut().find(|(s, _, _)| *s == ssrc) {
			if entry.1 != user {
				entry.1 = user;
				entry.2 = Assembler::default();
				self.rtx.retain(|(_, media)| *media != ssrc);
			}
			return Ok(());
		}
		if self.sources.len() >= MAX_SOURCES {
			return Err("Voice channel announces more video sources than supported");
		}
		self.sources.push((ssrc, user, Assembler::default()));
		self.require_keyframe(user);
		Ok(())
	}
	pub fn remove(&mut self, user: u64) {
		self.retain_user_sources(user, &[]);
	}
	/// Reconcile a complete stream announcement without resetting unchanged assemblers.
	pub fn retain_user_sources(&mut self, user: u64, keep: &[u32]) {
		self.sources
			.retain(|(ssrc, owner, _)| *owner != user || keep.contains(ssrc));
		self.rtx
			.retain(|(_, media)| self.sources.iter().any(|(ssrc, _, _)| ssrc == media));
		if !self.sources.iter().any(|(_, owner, _)| *owner == user) {
			self.awaiting_keyframe.retain(|u| *u != user);
		}
	}
	pub fn announce_rtx(&mut self, media: u32, rtx: u32) -> Result<(), &'static str> {
		if media == 0 || rtx == 0 {
			self.rtx.retain(|(_, source)| *source != media);
			return Ok(());
		}
		if !self.sources.iter().any(|(ssrc, _, _)| *ssrc == media)
			|| self.sources.iter().any(|(ssrc, _, _)| *ssrc == rtx)
			|| self
				.rtx
				.iter()
				.any(|(known, source)| *known == rtx && *source != media)
		{
			return Err("Invalid video retransmission SSRC");
		}
		self.rtx.retain(|(_, source)| *source != media);
		if self.rtx.len() >= MAX_SOURCES {
			return Err("Too many video retransmission sources");
		}
		self.rtx.push((rtx, media));
		Ok(())
	}
	/// RFC 4588: RTX payload starts with the original sequence number.
	pub fn restore_rtx(&self, ssrc: u32, payload: &mut Vec<u8>) -> Option<(u32, u16)> {
		let (_, media) = self.rtx.iter().find(|(rtx, _)| *rtx == ssrc)?;
		if payload.len() < 3 {
			return None;
		}
		let sequence = u16::from_be_bytes([payload[0], payload[1]]);
		payload.drain(..2);
		Some((*media, sequence))
	}
	/// A dropped or undecodable picture invalidates every later prediction until an IDR.
	pub fn require_keyframe(&mut self, user: u64) {
		if !self.awaiting_keyframe.contains(&user) {
			self.awaiting_keyframe.push(user);
		}
	}
	/// Adopt decoder-side losses so a keyframe gets requested for them too.
	pub fn absorb(&mut self, lost: &Lost) {
		if let Ok(mut lost) = lost.try_lock() {
			for user in lost.drain(..) {
				self.require_keyframe(user);
			}
		}
	}
	/// One video SSRC per user still waiting for a keyframe, for Picture Loss Indications.
	pub fn keyframe_requests(&self) -> impl Iterator<Item = u32> + '_ {
		self.awaiting_keyframe.iter().filter_map(|user| {
			self.sources
				.iter()
				.find(|(_, u, _)| u == user)
				.map(|(ssrc, _, _)| *ssrc)
		})
	}
	/// Whether any sender still owes this receiver a keyframe.
	pub fn awaiting(&self) -> bool {
		!self.awaiting_keyframe.is_empty()
	}
	/// Whether any video source has been announced for this connection.
	pub fn has_sources(&self) -> bool {
		!self.sources.is_empty()
	}
	/// Ask every announced sender for a keyframe. Used when video stops without the
	/// depacketizer seeing loss, which no per-picture signal would ever reveal.
	pub fn require_all_keyframes(&mut self) {
		for index in 0..self.sources.len() {
			self.require_keyframe(self.sources[index].1);
		}
	}
	/// Drains the depacketizer counters.
	pub fn take_stats(&mut self) -> ReceiveStats {
		std::mem::take(&mut self.stats)
	}
	/// Whether a decoded access unit may be decoded: keyframes always, predictions only
	/// while the reference chain is intact.
	pub fn accept(&mut self, user: u64, keyframe: bool) -> bool {
		if keyframe {
			self.awaiting_keyframe.retain(|u| *u != user);
			return true;
		}
		!self.awaiting_keyframe.contains(&user)
	}
	/// Fork: like `push`, but through the per-source sequence buffer, so a gap waits for its
	/// retransmission. Several pictures can complete at once when a gap fills.
	pub fn push_buffered(
		&mut self,
		ssrc: u32,
		sequence: u16,
		timestamp: u32,
		marker: bool,
		payload: &[u8],
		now: Instant,
	) -> Vec<(u64, Vec<u8>)> {
		let Some((_, user, assembler)) = self.sources.iter_mut().find(|(s, _, _)| *s == ssrc)
		else {
			self.stats.unknown_ssrc += 1;
			return Vec::new();
		};
		let user = *user;
		let released = assembler
			.reorder
			.insert(sequence, timestamp, marker, payload, now);
		let mut frames = Vec::new();
		let mut lost = false;
		for (sequence, timestamp, marker, payload) in released {
			if let Some(frame) = assembler.push(sequence, timestamp, marker, &payload) {
				frames.push((user, frame));
			}
			lost |= std::mem::take(&mut assembler.lost);
		}
		self.stats.incomplete += u64::from(lost);
		self.stats.complete += frames.len() as u64;
		if lost {
			self.require_keyframe(user);
		}
		frames
	}
	/// Missing packets to request, per media SSRC.
	pub fn nacks(&mut self, now: Instant) -> Vec<(u32, Vec<u16>)> {
		self.sources
			.iter_mut()
			.filter_map(|(ssrc, _, assembler)| {
				let due = assembler.reorder.nacks(now);
				(!due.is_empty()).then_some((*ssrc, due))
			})
			.collect()
	}
	/// Returns the owning user and a complete encrypted access unit when one closes.
	pub fn push(
		&mut self,
		ssrc: u32,
		sequence: u16,
		timestamp: u32,
		marker: bool,
		payload: &[u8],
	) -> Option<(u64, Vec<u8>)> {
		let Some((_, user, assembler)) = self.sources.iter_mut().find(|(s, _, _)| *s == ssrc)
		else {
			self.stats.unknown_ssrc += 1;
			return None;
		};
		let user = *user;
		let frame = assembler.push(sequence, timestamp, marker, payload);
		let lost = std::mem::take(&mut assembler.lost);
		self.stats.incomplete += u64::from(lost);
		self.stats.complete += u64::from(frame.is_some());
		if lost {
			self.require_keyframe(user);
		}
		frame.map(|frame| (user, frame))
	}
}

/// Decoder thread: cleartext access units in, bounded RGBA frames out through the sink.
/// Dropping the returned sender ends the thread and releases every decoder.
pub(crate) fn spawn_decoder(sink: VideoSink) -> Result<(DecoderQueue, Lost), &'static str> {
	// Keep short bursts, but recover from a fresh keyframe instead of replaying stale video.
	let (send, receive) = sync_channel(16);
	let lost: Lost = Arc::new(Mutex::new(Vec::new()));
	let report = lost.clone();
	let counters = Arc::new(DecoderCounters::default());
	let thread_counters = counters.clone();
	#[cfg(test)]
	let (completed, completion) = sync_channel(1);
	let worker = std::thread::Builder::new()
		.name("remote-video".into())
		.spawn(move || {
			decode_loop(receive, sink, report, thread_counters, true);
			// Signal only after every decoder and native runtime has been released.
			#[cfg(test)]
			let _ = completed.send(());
		})
		.map_err(|_| "Could not start the video decoder thread")?;
	#[cfg(not(test))]
	drop(worker);
	Ok((
		DecoderQueue {
			send,
			#[cfg(test)]
			worker: Some((worker, completion)),
			bytes: Arc::new(tokio::sync::Semaphore::new(QUEUE_BYTES)),
			active: Mutex::new(HashMap::new()),
			counters,
		},
		lost,
	))
}

/// RFC 4585 Picture Loss Indication asking the media server for a fresh keyframe.
pub(crate) fn pli(sender: u32, media: u32) -> ([u8; 8], [u8; 4]) {
	let mut header = [0x81, 206, 0, 2, 0, 0, 0, 0];
	header[4..].copy_from_slice(&sender.to_be_bytes());
	(header, media.to_be_bytes())
}

/// Queue a frame without blocking the transport. `Ok(false)` means the queue was full and
/// the frame dropped, so the caller must wait for the next keyframe.
pub(crate) fn offer(sender: &DecoderQueue, frame: Encoded) -> Result<bool, &'static str> {
	if frame.data.len() > MAX_FRAME_BYTES || frame.data.capacity() > QUEUE_BYTES {
		return Ok(false);
	}
	let Ok(permit) = sender
		.bytes
		.clone()
		.try_acquire_many_owned(frame.data.capacity() as u32)
	else {
		return Ok(false);
	};
	let active = {
		let mut users = sender
			.active
			.lock()
			.map_err(|_| "Video decoder state unavailable")?;
		if !users.contains_key(&frame.user) && users.len() >= MAX_SOURCES {
			return Ok(false);
		}
		users
			.entry(frame.user)
			.or_insert_with(|| Arc::new(AtomicBool::new(true)))
			.clone()
	};
	match sender
		.send
		.try_send(Decode::Frame(frame, permit, Instant::now(), active))
	{
		Ok(()) => Ok(true),
		Err(TrySendError::Full(_)) => Ok(false),
		Err(TrySendError::Disconnected(_)) => Err("Video decoder stopped"),
	}
}

/// Release one participant's decoder without blocking the media transport.
pub(crate) fn remove(sender: &DecoderQueue, user: u64) {
	if let Ok(mut users) = sender.active.lock()
		&& let Some(active) = users.remove(&user)
	{
		active.store(false, Ordering::Release);
	}
	// A full queue already wakes the worker; cancellation lives outside that queue.
	let _ = sender.send.try_send(Decode::Wake);
}

/// Drop decoders whose final SSRC disappeared, including SSRCs reassigned to another user.
pub(crate) fn retain_sources(sender: &DecoderQueue, receivers: &Receivers) {
	let mut cancelled = false;
	if let Ok(mut users) = sender.active.lock() {
		users.retain(|user, active| {
			let retained = receivers.sources.iter().any(|(_, owner, _)| owner == user);
			if !retained {
				active.store(false, Ordering::Release);
				cancelled = true;
			}
			retained
		});
	}
	if cancelled {
		let _ = sender.send.try_send(Decode::Wake);
	}
}

#[derive(Debug, PartialEq, Eq)]
enum DecodeError {
	/// The stream or decoder is unusable; the decoder is rebuilt at the next keyframe.
	Failed,
	/// The native queue is full; this access unit was dropped but the decoder is healthy.
	Busy,
}

/// Skip a user's predictions until a keyframe and report the loss so a PLI gets sent.
fn mark_broken(user: u64, broken: &mut Vec<u64>, lost: &Lost) {
	if !broken.contains(&user) && broken.len() < MAX_SOURCES {
		broken.push(user);
	}
	if let Ok(mut lost) = lost.lock()
		&& !lost.contains(&user)
		&& lost.len() < MAX_SOURCES
	{
		lost.push(user);
	}
}

/// Hardware decoding where the OS offers it; the software decoder is the fallback and the
/// only option on the other platforms. Hardware pictures reach the sink asynchronously.
enum Backend {
	/// The counter tracks pictures this decoder delivered, so a hardware path that accepts
	/// input but never outputs can be replaced by software.
	Hardware(
		platform::video::live::H264Decoder,
		Arc<std::sync::atomic::AtomicU64>,
	),
	Software(openh264::decoder::Decoder),
}
impl Backend {
	fn new(
		prefer_hardware: bool,
		user: u64,
		sink: &VideoSink,
		counters: &Arc<DecoderCounters>,
		lifetime: &Arc<AtomicBool>,
	) -> Option<Self> {
		if prefer_hardware {
			let sink = sink.clone();
			let counters = counters.clone();
			let lifetime = lifetime.clone();
			let delivered = Arc::new(std::sync::atomic::AtomicU64::new(0));
			let seen = delivered.clone();
			let deliver: platform::video::LiveSink = Box::new(move |frame| {
				if lifetime.load(Ordering::Acquire)
					&& bounded(frame.width as usize, frame.height as usize).is_ok()
				{
					seen.fetch_add(1, Ordering::Relaxed);
					counters.pictures.fetch_add(1, Ordering::Relaxed);
					sink(RemoteFrame {
						user,
						picture: frame.borrow(),
					});
				}
			});
			if let Ok(decoder) = platform::video::live::H264Decoder::new(deliver) {
				return Some(Self::Hardware(decoder, delivered));
			}
		}
		openh264::decoder::Decoder::new().ok().map(Self::Software)
	}
	/// Feed one access unit. Software pictures are returned; hardware ones were already
	/// delivered to the sink. `scratch` is reused so no frame-sized buffer is zeroed per frame.
	fn decode(
		&mut self,
		data: &[u8],
		scratch: &mut Vec<u8>,
	) -> Result<Option<(u32, u32)>, DecodeError> {
		match self {
			Self::Hardware(decoder, _) => decoder.decode(data).map(|()| None).map_err(|error| {
				if error == platform::video::BUSY {
					DecodeError::Busy
				} else {
					DecodeError::Failed
				}
			}),
			Self::Software(decoder) => {
				let decoded = match decoder.decode(data) {
					Ok(Some(yuv)) => yuv,
					Ok(None) => return Ok(None),
					Err(_) => return Err(DecodeError::Failed),
				};
				use openh264::formats::YUVSource;
				let (width, height) = decoded.dimensions();
				let (w, h) = bounded(width, height).map_err(|()| DecodeError::Failed)?;
				let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
				let (luma_stride, cb_stride, cr_stride) = decoded.strides();
				// Shared across participants: clearing keeps the capacity when resolutions
				// alternate, and the planes are copied as-is (I420) for GPU colour conversion.
				scratch.clear();
				let bytes = platform::video::Layout::I420
					.len(w, h)
					.ok_or(DecodeError::Failed)?;
				// After `clear`, the exact reservation covers the whole picture.
				scratch.reserve_exact(bytes);
				platform::video::pack_plane(scratch, decoded.y(), luma_stride, width, height)
					.and_then(|()| {
						platform::video::pack_plane(
							scratch,
							decoded.u(),
							cb_stride,
							chroma_width,
							chroma_height,
						)
					})
					.and_then(|()| {
						platform::video::pack_plane(
							scratch,
							decoded.v(),
							cr_stride,
							chroma_width,
							chroma_height,
						)
					})
					.ok_or(DecodeError::Failed)?;
				Ok(Some((w, h)))
			}
		}
	}
	#[cfg(all(test, target_os = "macos"))]
	fn flush(&self) {
		if let Self::Hardware(decoder, _) = self {
			decoder.flush();
		}
	}
	/// Pictures a hardware decoder delivered so far; software reports pictures directly.
	fn hardware_pictures(&self) -> Option<u64> {
		match self {
			Self::Hardware(_, delivered) => Some(delivered.load(Ordering::Relaxed)),
			Self::Software(_) => None,
		}
	}
}
/// Keyframes a hardware decoder may swallow without a single picture before software
/// takes over (fork: some GPU decoders accept 1440p/4K input and never output).
const SILENT_HARDWARE_KEYFRAMES: u32 = 3;

fn decode_loop(
	receive: Receiver<Decode>,
	sink: VideoSink,
	lost: Lost,
	counters: Arc<DecoderCounters>,
	prefer_hardware: bool,
) {
	let mut decoders: HashMap<u64, Backend> = HashMap::new();
	let mut active: HashMap<u64, Arc<AtomicBool>> = HashMap::new();
	// Users whose hardware decoder rejected the stream fall back to software.
	let mut software_only: Vec<u64> = Vec::new();
	// After a decode error, predictions are skipped until a keyframe rebuilds the references.
	let mut broken: Vec<u64> = Vec::new();
	// Keyframes fed to a hardware decoder that has not delivered a picture yet.
	let mut silent: HashMap<u64, u32> = HashMap::new();
	// One log line per user and outcome, so a stuck stream is explained in session.log.
	let mut logged: Vec<(u64, &'static str)> = Vec::new();
	let mut note = |user: u64, what: &'static str| {
		if !logged.contains(&(user, what)) && logged.len() < 64 {
			logged.push((user, what));
			client_core::session_log::record(&format!("Stream video: {what}"));
		}
	};
	let mut scratch = Vec::new();
	while let Ok(message) = receive.recv() {
		let had_active = !active.is_empty();
		active.retain(|user, lifetime| {
			let retained = lifetime.load(Ordering::Acquire);
			if !retained {
				decoders.remove(user);
				software_only.retain(|known| known != user);
				broken.retain(|known| known != user);
				silent.remove(user);
			}
			retained
		});
		// Only cancellation of the final lifetime releases scratch; decode errors keep reuse.
		if had_active && active.is_empty() {
			scratch = Vec::new();
		}
		decoder_counts(&decoders, &counters);
		let (frame, _permit, queued, lifetime) = match message {
			Decode::Frame(frame, permit, queued, lifetime) => (frame, permit, queued, lifetime),
			Decode::Wake => continue,
			#[cfg(test)]
			Decode::Barrier(done) => {
				let _ = done.send(scratch.capacity());
				continue;
			}
		};
		if !lifetime.load(Ordering::Acquire) || frame.data.len() > MAX_FRAME_BYTES {
			continue;
		}
		active.insert(frame.user, lifetime.clone());
		let age = queued.elapsed();
		counters.queue_ms.fetch_max(
			age.as_millis().min(u128::from(u64::MAX)) as u64,
			Ordering::Relaxed,
		);
		// A late keyframe is still decoded: it is the only thing that restarts the picture,
		// and dropping it on a slow decoder made large streams loop on keyframe requests.
		if age > MAX_DECODE_AGE && !frame.keyframe {
			counters.stale.fetch_add(1, Ordering::Relaxed);
			decoders.remove(&frame.user);
			decoder_counts(&decoders, &counters);
			mark_broken(frame.user, &mut broken, &lost);
			continue;
		}
		if frame.keyframe {
			broken.retain(|user| *user != frame.user);
		} else if broken.contains(&frame.user) {
			continue;
		}
		// A hardware decoder that keeps swallowing keyframes without output is dropped here,
		// so this keyframe goes straight to the software decoder created below.
		if frame.keyframe
			&& let Some(pictures) = decoders
				.get(&frame.user)
				.and_then(Backend::hardware_pictures)
		{
			if pictures > 0 {
				silent.remove(&frame.user);
			} else {
				let count = silent.entry(frame.user).or_default();
				*count += 1;
				if *count > SILENT_HARDWARE_KEYFRAMES {
					note(
						frame.user,
						"hardware decoder produced no picture; switched to software",
					);
					silent.remove(&frame.user);
					decoders.remove(&frame.user);
					if !software_only.contains(&frame.user) {
						software_only.push(frame.user);
					}
					decoder_counts(&decoders, &counters);
				}
			}
		}
		if !decoders.contains_key(&frame.user) {
			if decoders.len() >= MAX_DECODERS {
				continue;
			}
			let Some(decoder) = Backend::new(
				prefer_hardware && !software_only.contains(&frame.user),
				frame.user,
				&sink,
				&counters,
				&lifetime,
			) else {
				counters.errors.fetch_add(1, Ordering::Relaxed);
				continue;
			};
			decoders.insert(frame.user, decoder);
		}
		let decoder = decoders.get_mut(&frame.user).expect("decoder inserted");
		let hardware = !matches!(decoder, Backend::Software(_));
		let decoded = match decoder.decode(&frame.data, &mut scratch) {
			Ok(Some(picture)) => picture,
			Ok(None) => {
				decoder_counts(&decoders, &counters);
				continue;
			}
			Err(DecodeError::Busy) => {
				// Backpressure, not a decoder fault: keep the decoder and its hardware path,
				// and skip predictions until the requested keyframe arrives.
				counters.stale.fetch_add(1, Ordering::Relaxed);
				decoder_counts(&decoders, &counters);
				mark_broken(frame.user, &mut broken, &lost);
				continue;
			}
			Err(DecodeError::Failed) => {
				// Corrupt or lost data: a fresh decoder waits for the next keyframe. A
				// hardware decoder that fails on a keyframe is replaced by software.
				note(
					frame.user,
					if hardware {
						"hardware decode failed"
					} else {
						"software decode failed (resolution or data rejected)"
					},
				);
				counters.errors.fetch_add(1, Ordering::Relaxed);
				decoders.remove(&frame.user);
				decoder_counts(&decoders, &counters);
				if hardware && frame.keyframe && !software_only.contains(&frame.user) {
					software_only.push(frame.user);
				}
				mark_broken(frame.user, &mut broken, &lost);
				continue;
			}
		};
		decoder_counts(&decoders, &counters);
		let (width, height) = decoded;
		if !lifetime.load(Ordering::Acquire) {
			continue;
		}
		note(frame.user, "first software picture decoded");
		counters.pictures.fetch_add(1, Ordering::Relaxed);
		sink(RemoteFrame {
			user: frame.user,
			picture: platform::video::Picture {
				width,
				height,
				layout: platform::video::Layout::I420,
				color: platform::video::Color::WEBRTC,
				rotation: 0,
				data: &scratch,
			},
		});
	}
	// Hardware pictures still in flight must land before the sink goes away.
	for decoder in decoders.values() {
		if let Backend::Hardware(decoder, _) = decoder {
			decoder.flush();
		}
	}
}

fn decoder_counts(decoders: &HashMap<u64, Backend>, counters: &DecoderCounters) {
	let hardware = decoders
		.values()
		.filter(|decoder| matches!(decoder, Backend::Hardware(..)))
		.count();
	counters.hardware.store(hardware as u64, Ordering::Relaxed);
	counters
		.software
		.store((decoders.len() - hardware) as u64, Ordering::Relaxed);
}

fn bounded(width: usize, height: usize) -> Result<(u32, u32), ()> {
	let (w, h) = (
		u32::try_from(width).map_err(|_| ())?,
		u32::try_from(height).map_err(|_| ())?,
	);
	if w == 0
		|| h == 0
		|| w > MAX_WIDTH
		|| h > MAX_WIDTH
		|| u64::from(w) * u64::from(h) > MAX_PIXELS
	{
		return Err(());
	}
	Ok((w, h))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn decoder_cleanup_queue(capacity: usize) -> (DecoderQueue, Receiver<Decode>) {
		let (send, receive) = sync_channel(capacity);
		(
			DecoderQueue {
				send,
				worker: None,
				bytes: Arc::new(tokio::sync::Semaphore::new(QUEUE_BYTES)),
				active: Mutex::new(HashMap::new()),
				counters: Arc::default(),
			},
			receive,
		)
	}

	fn decoder_cleanup_keyframe(width: usize, height: usize) -> Vec<u8> {
		use openh264::{
			OpenH264API,
			encoder::{Encoder, EncoderConfig},
			formats::YUVBuffer,
		};
		let mut encoder =
			Encoder::with_api_config(OpenH264API::from_source(), EncoderConfig::new()).unwrap();
		let mut data = Vec::new();
		encoder
			.encode(&YUVBuffer::new(width, height))
			.unwrap()
			.write_vec(&mut data);
		data
	}

	fn decoder_cleanup_sync(queue: &DecoderQueue) -> usize {
		let (send, receive) = sync_channel(1);
		assert!(queue.send.send(Decode::Barrier(send)).is_ok());
		receive.recv_timeout(Duration::from_secs(10)).unwrap()
	}

	/// Decodes one keyframe, resending it when a loaded runner held it past `MAX_DECODE_AGE`.
	fn decoder_cleanup_decode(queue: &DecoderQueue, user: u64, data: &[u8]) {
		let deadline = Instant::now() + Duration::from_secs(10);
		loop {
			let stale = queue.counters.stale.load(Ordering::Relaxed);
			assert!(
				offer(
					queue,
					Encoded {
						user,
						data: data.to_vec(),
						keyframe: true
					}
				)
				.unwrap()
			);
			decoder_cleanup_sync(queue);
			if queue.counters.stale.load(Ordering::Relaxed) == stale {
				return;
			}
			assert!(Instant::now() < deadline, "keyframe stayed stale");
		}
	}

	#[test]
	fn decoder_cleanup_invalidates_full_queue_and_allows_restart() {
		let data = decoder_cleanup_keyframe(32, 32);
		for full in [true, false] {
			let (queue, receive) = decoder_cleanup_queue(3);
			for _ in 0..if full { 3 } else { 1 } {
				assert!(
					offer(
						&queue,
						Encoded {
							user: 7,
							data: data.clone(),
							keyframe: true
						}
					)
					.unwrap()
				);
			}
			remove(&queue, 7);
			if !full {
				assert!(
					offer(
						&queue,
						Encoded {
							user: 7,
							data: data.clone(),
							keyframe: true
						}
					)
					.unwrap()
				);
			}
			let bytes = queue.bytes.clone();
			let counters = queue.counters.clone();
			drop(queue);
			let seen = Arc::new(Mutex::new(Vec::new()));
			let pictures = seen.clone();
			decode_loop(
				receive,
				Arc::new(move |frame| pictures.lock().unwrap().push(frame.user)),
				Arc::default(),
				counters.clone(),
				false,
			);
			assert_eq!(*seen.lock().unwrap(), if full { vec![] } else { vec![7] });
			assert_eq!(counters.errors.load(Ordering::Relaxed), 0);
			assert_eq!(counters.software.load(Ordering::Relaxed), u64::from(!full));
			assert_eq!(bytes.available_permits(), QUEUE_BYTES);
		}
	}

	#[test]
	fn decoder_cleanup_full_queue_rapid_off_on_keeps_only_the_new_lifetime() {
		let data = decoder_cleanup_keyframe(32, 32);
		let (queue, receive) = decoder_cleanup_queue(3);
		let (seen, pictures) = sync_channel(8);
		let (resume, paused) = sync_channel(1);
		let paused = Mutex::new(paused);
		let counters = queue.counters.clone();
		let worker = std::thread::spawn(move || {
			decode_loop(
				receive,
				Arc::new(move |frame| {
					seen.send(frame.user).unwrap();
					if frame.user == 1 {
						paused
							.lock()
							.unwrap()
							.recv_timeout(Duration::from_secs(10))
							.unwrap();
					}
				}),
				Arc::default(),
				counters,
				false,
			)
		});
		assert!(
			offer(
				&queue,
				Encoded {
					user: 1,
					data: data.clone(),
					keyframe: true
				}
			)
			.unwrap()
		);
		assert_eq!(pictures.recv_timeout(Duration::from_secs(10)).unwrap(), 1);
		// The first sink is paused, so all three old frames and the failed wake are deterministic.
		for _ in 0..3 {
			assert!(
				offer(
					&queue,
					Encoded {
						user: 7,
						data: data.clone(),
						keyframe: true
					}
				)
				.unwrap()
			);
		}
		remove(&queue, 7);
		// Restart before the old queue drains; the first offer creates a fresh lifetime.
		assert!(
			!offer(
				&queue,
				Encoded {
					user: 7,
					data: data.clone(),
					keyframe: true
				}
			)
			.unwrap()
		);
		resume.send(()).unwrap();
		let deadline = Instant::now() + Duration::from_secs(10);
		while !offer(
			&queue,
			Encoded {
				user: 7,
				data: data.clone(),
				keyframe: true,
			},
		)
		.unwrap()
		{
			assert!(Instant::now() < deadline, "decoder did not drain its queue");
			std::thread::yield_now();
		}
		decoder_cleanup_sync(&queue);
		assert_eq!(pictures.try_iter().collect::<Vec<_>>(), vec![7]);
		assert_eq!(queue.counters.software.load(Ordering::Relaxed), 2);
		drop(queue);
		worker.join().unwrap();
	}

	#[test]
	fn decoder_cleanup_tracks_all_sources_and_reassigned_owners() {
		let (queue, receive) = decoder_cleanup_queue(16);
		let mut receivers = Receivers::default();
		receivers.announce(7, 700).unwrap();
		receivers.announce(7, 710).unwrap();
		assert!(
			offer(
				&queue,
				Encoded {
					user: 7,
					data: vec![0],
					keyframe: true
				}
			)
			.unwrap()
		);
		let original = queue.active.lock().unwrap()[&7].clone();
		assert!(matches!(receive.try_recv(), Ok(Decode::Frame(..))));
		retain_sources(&queue, &receivers);
		assert!(matches!(
			receive.try_recv(),
			Err(std::sync::mpsc::TryRecvError::Empty)
		));
		// Reassigning one of a user's sources must not cancel their remaining source.
		receivers.announce(8, 700).unwrap();
		retain_sources(&queue, &receivers);
		assert!(original.load(Ordering::Acquire));
		assert!(matches!(
			receive.try_recv(),
			Err(std::sync::mpsc::TryRecvError::Empty)
		));
		// Reassigning the final SSRC must also release its previous owner's decoder.
		receivers.announce(8, 710).unwrap();
		retain_sources(&queue, &receivers);
		assert!(!original.load(Ordering::Acquire));
		assert!(!queue.active.lock().unwrap().contains_key(&7));
		assert!(matches!(receive.try_recv(), Ok(Decode::Wake)));
		assert!(
			offer(
				&queue,
				Encoded {
					user: 8,
					data: vec![0],
					keyframe: true
				}
			)
			.unwrap()
		);
		let current = queue.active.lock().unwrap()[&8].clone();
		receivers.announce(8, 0).unwrap();
		retain_sources(&queue, &receivers);
		assert!(!current.load(Ordering::Acquire));
	}

	#[test]
	fn decoder_cleanup_releases_slots_for_another_participant() {
		let data = decoder_cleanup_keyframe(32, 32);
		let (queue, receive) = decoder_cleanup_queue(16);
		let counters = queue.counters.clone();
		let seen = Arc::new(Mutex::new(Vec::new()));
		let pictures = seen.clone();
		let worker = std::thread::spawn(move || {
			decode_loop(
				receive,
				Arc::new(move |frame| pictures.lock().unwrap().push(frame.user)),
				Arc::default(),
				counters,
				false,
			)
		});
		let mut receivers = Receivers::default();
		for user in 1..=MAX_DECODERS as u64 {
			crate::transport::announce_video(
				&mut receivers,
				&queue,
				user,
				&serde_json::json!({"video_ssrc": user}),
			)
			.unwrap();
			decoder_cleanup_decode(&queue, user, &data);
		}
		assert_eq!(
			queue.counters.software.load(Ordering::Relaxed),
			MAX_DECODERS as u64
		);
		crate::transport::announce_video(
			&mut receivers,
			&queue,
			1,
			&serde_json::json!({"video_ssrc": 0, "streams": [{"type": "video", "ssrc": 1}]}),
		)
		.unwrap();
		decoder_cleanup_sync(&queue);
		assert_eq!(
			queue.counters.software.load(Ordering::Relaxed),
			MAX_DECODERS as u64
		);
		crate::transport::announce_video(
			&mut receivers,
			&queue,
			1,
			&serde_json::json!({"video_ssrc": 0, "streams": []}),
		)
		.unwrap();
		decoder_cleanup_sync(&queue);
		assert_eq!(
			queue.counters.software.load(Ordering::Relaxed),
			MAX_DECODERS as u64 - 1
		);
		crate::transport::announce_video(
			&mut receivers,
			&queue,
			9,
			&serde_json::json!({"video_ssrc": 9}),
		)
		.unwrap();
		decoder_cleanup_decode(&queue, 9, &data);
		assert_eq!(seen.lock().unwrap().last(), Some(&9));
		assert_eq!(
			queue.counters.software.load(Ordering::Relaxed),
			MAX_DECODERS as u64
		);
		assert!(decoder_cleanup_sync(&queue) > 0);
		for user in 2..=9 {
			crate::transport::announce_video(
				&mut receivers,
				&queue,
				user,
				&serde_json::json!({"video_ssrc": 0, "streams": []}),
			)
			.unwrap();
		}
		assert_eq!(
			decoder_cleanup_sync(&queue),
			0,
			"final camera-off releases scratch"
		);
		assert_eq!(queue.counters.software.load(Ordering::Relaxed), 0);
		drop(queue);
		worker.join().unwrap();
	}

	#[test]
	fn parameter_set_detection_needs_both_sps_and_pps() {
		assert!(has_parameter_sets(&[
			0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3
		]));
		assert!(!has_parameter_sets(&[
			0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x65, 3
		]));
		assert!(!has_parameter_sets(&[0, 0, 0, 1, 0x65, 3]));
		assert!(!has_parameter_sets(&[]));
	}

	#[test]
	fn single_stap_and_fragmented_nal_units_rebuild_annex_b() {
		let mut assembler = Assembler::default();
		// SPS + PPS aggregated, then a fragmented IDR slice, all in one picture.
		let stap = [24, 0, 2, 0x67, 0xaa, 0, 1, 0x68];
		assert!(assembler.push(10, 900, false, &stap).is_none());
		let start = [0x7c, 0x85, 1, 2, 3];
		let middle = [0x7c, 0x05, 4, 5];
		let end = [0x7c, 0x45, 6];
		assert!(assembler.push(11, 900, false, &start).is_none());
		assert!(assembler.push(12, 900, false, &middle).is_none());
		let frame = assembler.push(13, 900, true, &end).unwrap();
		assert_eq!(
			frame,
			vec![
				0, 0, 0, 1, 0x67, 0xaa, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65, 1, 2, 3, 4, 5, 6
			]
		);
		// A single NAL picture after a lost packet is dropped, the next intact one is kept.
		assert!(assembler.push(15, 1800, true, &[0x41, 9]).is_none());
		assert_eq!(
			assembler.push(16, 2700, true, &[0x41, 9]).unwrap(),
			vec![0, 0, 0, 1, 0x41, 9]
		);
		// Unknown NAL types and oversized frames never complete.
		assert!(assembler.push(17, 3600, true, &[29, 1]).is_none());
		let mut huge = Assembler::default();
		let chunk = vec![7u8; 1200];
		let mut sequence = 0u16;
		for _ in 0..(MAX_FRAME_BYTES / 1200 + 2) {
			assert!(huge.push(sequence, 1, false, &chunk).is_none());
			sequence = sequence.wrapping_add(1);
		}
		assert!(huge.push(sequence, 1, true, &chunk).is_none());
	}
	#[test]
	fn packet_loss_requests_a_keyframe_before_accepting_predictions() {
		for missing_marker in [false, true] {
			let mut receivers = Receivers::default();
			receivers.announce(7, 700).unwrap();
			assert!(receivers.push(700, 1, 900, true, &[0x65, 1]).is_some());
			assert!(receivers.accept(7, true));
			assert!(receivers.keyframe_requests().next().is_none());
			if missing_marker {
				assert!(receivers.push(700, 2, 1800, false, &[0x41, 1]).is_none());
				// A new timestamp exposes an unfinished prior picture even without a sequence gap.
				assert!(receivers.push(700, 3, 2700, true, &[0x41, 2]).is_some());
			} else {
				assert!(receivers.push(700, 3, 1800, true, &[0x41, 1]).is_none());
			}
			assert_eq!(receivers.keyframe_requests().collect::<Vec<_>>(), vec![700]);
			assert!(!receivers.accept(7, false));
			assert!(receivers.push(700, 4, 3600, true, &[0x41, 3]).is_some());
			assert!(!receivers.accept(7, false));
			assert!(receivers.push(700, 5, 4500, true, &[0x65, 4]).is_some());
			assert!(receivers.accept(7, true));
			assert!(receivers.keyframe_requests().next().is_none());
			assert!(receivers.accept(7, false));
		}

		{
			let mut receivers = Receivers::default();
			receivers.announce(7, 700).unwrap();
			assert!(receivers.push(999, 1, 900, true, &[0x65, 1]).is_none());
			assert!(receivers.push(700, 1, 900, true, &[0x65, 1]).is_some());
			assert!(receivers.push(700, 3, 1800, true, &[0x41, 1]).is_none());
			let stats = receivers.take_stats();
			assert_eq!(
				(stats.unknown_ssrc, stats.incomplete, stats.complete),
				(1, 1, 1)
			);
			assert!(receivers.awaiting());
			let stats = receivers.take_stats();
			assert_eq!(
				(stats.unknown_ssrc, stats.incomplete, stats.complete),
				(0, 0, 0)
			);
		}
	}
	#[test]
	fn receivers_bind_ssrcs_to_users_within_the_source_limit() {
		let mut receivers = Receivers::default();
		receivers.announce(5, 100).unwrap();
		receivers.announce(5, 101).unwrap();
		assert_eq!(
			receivers.push(100, 1, 1, true, &[0x41, 1]).unwrap(),
			(5, vec![0, 0, 0, 1, 0x41, 1])
		);
		assert!(receivers.push(999, 1, 1, true, &[0x41, 1]).is_none());
		receivers.announce(5, 0).unwrap();
		assert!(receivers.push(100, 2, 2, true, &[0x41, 1]).is_none());
		for ssrc in 1..=MAX_SOURCES as u32 {
			receivers.announce(u64::from(ssrc), ssrc).unwrap();
		}
		assert!(receivers.announce(99, 4242).is_err());
		// Predictions are gated until the first keyframe, and again after a drop.
		let mut gated = Receivers::default();
		gated.announce(7, 700).unwrap();
		assert!(!gated.accept(7, false));
		assert!(gated.accept(7, true));
		assert!(gated.accept(7, false));
		gated.require_keyframe(7);
		assert!(!gated.accept(7, false));
		assert!(is_keyframe(&[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 2]));
		assert!(!is_keyframe(&[0, 0, 0, 1, 0x41, 9]));
		// Fork: 1440p and 4K streams decode (upstream rejected anything past 1080p).
		assert!(bounded(2560, 1440).is_ok());
		assert!(bounded(3840, 2160).is_ok());
		assert!(bounded(7680, 4320).is_err());
		assert!(bounded(0, 4).is_err());
	}
	#[test]
	fn reorder_waits_for_a_retransmission_then_gives_up() {
		let start = Instant::now();
		let mut reorder = Reorder::default();
		let seqs =
			|out: Vec<(u16, u32, bool, Vec<u8>)>| out.iter().map(|p| p.0).collect::<Vec<_>>();
		assert_eq!(seqs(reorder.insert(10, 1, false, &[1], start)), [10]);
		// 11 is lost: 12 and 13 wait, and 11 is requested once per retry interval.
		assert!(reorder.insert(12, 1, false, &[3], start).is_empty());
		assert!(reorder.insert(13, 1, true, &[4], start).is_empty());
		assert_eq!(reorder.nacks(start), [11]);
		assert!(reorder.nacks(start).is_empty());
		assert_eq!(reorder.nacks(start + NACK_RETRY), [11]);
		// The retransmission releases the whole picture in order.
		assert_eq!(
			seqs(reorder.insert(11, 1, false, &[2], start)),
			[11, 12, 13]
		);
		// A gap that never fills is skipped after the wait.
		assert!(reorder.insert(15, 2, true, &[6], start).is_empty());
		assert_eq!(
			seqs(reorder.insert(16, 3, true, &[7], start + REORDER_WAIT)),
			[15, 16]
		);
		// Late duplicates of released packets are ignored.
		assert!(
			reorder
				.insert(11, 1, false, &[2], start + REORDER_WAIT)
				.is_empty()
		);
		// Sequence numbers wrap.
		let mut wrap = Reorder::default();
		assert_eq!(seqs(wrap.insert(65535, 9, false, &[1], start)), [65535]);
		assert_eq!(seqs(wrap.insert(0, 9, true, &[1], start)), [0]);
	}
	#[test]
	fn nack_packs_nearby_losses_into_one_entry() {
		let (header, body) = nack(7, 100, &[10, 11, 13, 40]).unwrap();
		assert_eq!(header[..2], [0x81, 205]);
		// Two FCI entries: 10 with 11 and 13 in its bitmask, then 40.
		assert_eq!(u16::from_be_bytes([header[2], header[3]]), 4);
		assert_eq!(body[..4], 100u32.to_be_bytes());
		assert_eq!(body[4..8], [0, 10, 0, 0b101]);
		assert_eq!(body[8..12], [0, 40, 0, 0]);
		assert!(nack(7, 100, &[]).is_none());
	}
	#[test]
	fn software_decoders_reuse_scratch_across_alternating_resolutions() {
		use openh264::{
			OpenH264API,
			encoder::{Encoder, EncoderConfig},
			formats::YUVBuffer,
		};
		let mut streams: Vec<_> = [(64, 64), (32, 32)]
			.into_iter()
			.map(|(width, height)| {
				let mut encoder =
					Encoder::with_api_config(OpenH264API::from_source(), EncoderConfig::new())
						.unwrap();
				let mut data = Vec::new();
				encoder
					.encode(&YUVBuffer::new(width, height))
					.unwrap()
					.write_vec(&mut data);
				let decoder = Backend::Software(openh264::decoder::Decoder::new().unwrap());
				(decoder, data, (width as u32, height as u32))
			})
			.collect();
		// Growing from the smaller picture still holds the whole larger one. `reserve_exact`
		// only promises a lower bound, so the exact capacity is left to the allocator.
		let mut grown = Vec::new();
		for index in [1, 0] {
			let (decoder, data, _) = &mut streams[index];
			assert!(decoder.decode(data, &mut grown).is_ok());
		}
		let bytes = platform::video::Layout::I420.len(64, 64).unwrap();
		assert!(grown.capacity() >= bytes);
		let mut scratch = Vec::new();
		let mut allocation = None;
		for _ in 0..4 {
			for (decoder, data, dimensions) in &mut streams {
				assert_eq!(decoder.decode(data, &mut scratch), Ok(Some(*dimensions)));
				// Exactly one packed I420 picture; the larger allocation is kept.
				let bytes = platform::video::Layout::I420
					.len(dimensions.0, dimensions.1)
					.unwrap();
				assert_eq!(scratch.len(), bytes);
				let current = (scratch.as_ptr(), scratch.capacity());
				assert_eq!(*allocation.get_or_insert(current), current);
			}
		}
	}

	/// `cargo test --release -p discord-voice compare_alternating_software_decode -- --ignored --nocapture`
	#[test]
	#[ignore]
	fn compare_alternating_software_decode() {
		use openh264::{
			OpenH264API,
			encoder::{Encoder, EncoderConfig},
			formats::YUVBuffer,
		};
		let streams: Vec<_> = [(1920, 1080), (1280, 720)]
			.into_iter()
			.map(|(width, height)| {
				let mut encoder =
					Encoder::with_api_config(OpenH264API::from_source(), EncoderConfig::new())
						.unwrap();
				let mut data = Vec::new();
				encoder
					.encode(&YUVBuffer::new(width, height))
					.unwrap()
					.write_vec(&mut data);
				data
			})
			.collect();
		for run in 0..6 {
			let mut decoders = streams
				.iter()
				.map(|_| Backend::Software(openh264::decoder::Decoder::new().unwrap()))
				.collect::<Vec<_>>();
			let mut scratch = Vec::new();
			let mut length_changes = 0;
			let mut peak_capacity = 0;
			let start = std::time::Instant::now();
			for _ in 0..60 {
				for (decoder, data) in decoders.iter_mut().zip(&streams) {
					let previous = scratch.len();
					let (width, height) = decoder.decode(data, &mut scratch).unwrap().unwrap();
					std::hint::black_box((width, height, &scratch[..]));
					length_changes += usize::from(previous != scratch.len());
					peak_capacity = peak_capacity.max(scratch.capacity());
				}
			}
			println!(
				"alternating software run={run} (0=warmup) frames=120 ms={:.3} scratch_length_changes={length_changes} peak_scratch_capacity={peak_capacity}",
				start.elapsed().as_secs_f64() * 1000.0
			);
		}
	}

	#[cfg(target_os = "macos")]
	#[test]
	fn hardware_decoder_round_trips_an_openh264_keyframe() {
		use openh264::{
			OpenH264API,
			encoder::{Encoder, EncoderConfig},
			formats::YUVBuffer,
		};
		let mut encoder =
			Encoder::with_api_config(OpenH264API::from_source(), EncoderConfig::new()).unwrap();
		let yuv = YUVBuffer::new(320, 240);
		let pictures = Arc::new(std::sync::Mutex::new(Vec::new()));
		let seen = pictures.clone();
		let sink: VideoSink = Arc::new(move |frame: RemoteFrame| {
			seen.lock().unwrap().push((
				frame.user,
				frame.picture.width,
				frame.picture.height,
				frame.picture.to_rgba().unwrap().2,
			));
		});
		let counters = Arc::new(DecoderCounters::default());
		let lifetime = Arc::new(AtomicBool::new(true));
		let mut decoder =
			Backend::new(true, 9, &sink, &counters, &lifetime).expect("hardware backend");
		assert!(matches!(decoder, Backend::Hardware(..)));
		let mut scratch = Vec::new();
		for _ in 0..3 {
			let encoded = encoder.encode(&yuv).unwrap();
			let mut data = Vec::new();
			encoded.write_vec(&mut data);
			assert!(!data.is_empty());
			assert!(decoder.decode(&data, &mut scratch).unwrap().is_none());
		}
		decoder.flush();
		let delivered = {
			let pictures = pictures.lock().unwrap();
			assert!(!pictures.is_empty(), "VideoToolbox produced pictures");
			let frame = &pictures[0];
			assert_eq!((frame.0, frame.1, frame.2), (9, 320, 240));
			assert_eq!(frame.3.len(), 320 * 240 * 4);
			assert!(frame.3.as_chunks::<4>().0.iter().all(|px| px[3] == 255));
			pictures.len() as u64
		};
		assert_eq!(counters.pictures.load(Ordering::Relaxed), delivered);
		lifetime.store(false, Ordering::Release);
		let mut data = Vec::new();
		encoder.encode(&yuv).unwrap().write_vec(&mut data);
		decoder.decode(&data, &mut scratch).unwrap();
		decoder.flush();
		assert_eq!(pictures.lock().unwrap().len() as u64, delivered);
		assert_eq!(
			counters.pictures.load(Ordering::Relaxed),
			delivered,
			"cancelled hardware callbacks must not reset the stall detector"
		);
	}
	/// `cargo test -p discord-voice compare_decoder_backends -- --ignored --nocapture`
	#[cfg(target_os = "macos")]
	#[test]
	#[ignore]
	fn compare_decoder_backends() {
		use openh264::{
			OpenH264API,
			encoder::{BitRate, Encoder, EncoderConfig},
			formats::YUVBuffer,
		};
		let mut encoder = Encoder::with_api_config(
			OpenH264API::from_source(),
			EncoderConfig::new().bitrate(BitRate::from_bps(6_000_000)),
		)
		.unwrap();
		let (width, height) = (1280usize, 720usize);
		let frames: Vec<Vec<u8>> = (0..60u8)
			.map(|i| {
				let mut yuv = vec![0u8; width * height * 3 / 2];
				for (n, byte) in yuv[..width * height].iter_mut().enumerate() {
					*byte = ((n / width) as u8).wrapping_add(i.wrapping_mul(3));
				}
				let mut data = Vec::new();
				encoder
					.encode(&YUVBuffer::from_vec(yuv, width, height))
					.unwrap()
					.write_vec(&mut data);
				data
			})
			.collect();
		for hardware in [true, false] {
			let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
			let seen = count.clone();
			let sink: VideoSink = Arc::new(move |_| {
				seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
			});
			let mut decoder = Backend::new(
				hardware,
				1,
				&sink,
				&Arc::default(),
				&Arc::new(AtomicBool::new(true)),
			)
			.unwrap();
			let mut scratch = Vec::new();
			// Session start-up (IOSurface, Metal) is a one-time cost; time steady state only.
			let _ = decoder.decode(&frames[0], &mut scratch);
			decoder.flush();
			count.store(0, std::sync::atomic::Ordering::Relaxed);
			let start = std::time::Instant::now();
			let mut pictures = 0;
			for frame in &frames[1..] {
				if let Ok(Some(_)) = decoder.decode(frame, &mut scratch) {
					pictures += 1;
				}
			}
			decoder.flush();
			let pictures = pictures + count.load(std::sync::atomic::Ordering::Relaxed);
			println!(
				"{} decoder: {pictures} pictures of 720p in {:?} ({:.2} ms per frame)",
				if hardware { "VideoToolbox" } else { "openh264" },
				start.elapsed(),
				start.elapsed().as_secs_f64() * 1000.0 / pictures.max(1) as f64
			);
		}
	}
	#[test]
	fn decoder_thread_rejects_garbage_and_ends_with_its_sender() {
		let frames = Arc::new(std::sync::Mutex::new(0usize));
		let seen = frames.clone();
		let (sender, lost) = spawn_decoder(Arc::new(move |_| *seen.lock().unwrap() += 1)).unwrap();
		assert!(
			offer(
				&sender,
				Encoded {
					user: 1,
					data: vec![0, 0, 0, 1, 0x65, 1, 2, 3],
					keyframe: true,
				},
			)
			.unwrap()
		);
		drop(sender);
		std::thread::sleep(std::time::Duration::from_millis(50));
		assert_eq!(*frames.lock().unwrap(), 0);
		// Garbage after a keyframe header is a decode failure the transport must learn about.
		let mut receivers = Receivers::default();
		receivers.announce(1, 100).unwrap();
		receivers.accept(1, true);
		receivers.absorb(&lost);
		assert_eq!(
			receivers.keyframe_requests().collect::<Vec<_>>(),
			if lost.lock().unwrap().is_empty() && !receivers.accept(1, false) {
				vec![100]
			} else {
				vec![]
			}
		);
		let (header, body) = pli(7, 100);
		assert_eq!(header, [0x81, 206, 0, 2, 0, 0, 0, 7]);
		assert_eq!(body, 100u32.to_be_bytes());
	}
}
