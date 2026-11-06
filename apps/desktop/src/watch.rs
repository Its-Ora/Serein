//! Desktop ownership for watching other participants' screen shares. Each choice is an
//! explicit click; frames are decoded off the render thread and never written to disk.
//! Fork: several streams can be watched at once, each with its own media connection; their
//! audio is summed into the call's single stream-audio input.
use client_core::{Command, Event, State, screen, voice};
use discord_voice::{Identity, Status};
use eframe::egui;
use model::Id;
use std::{
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::watch, task::JoinHandle};
use zeroize::Zeroizing;

const SIGNAL_TIMEOUT: Duration = Duration::from_secs(30);
// Eight UHD YUV pictures plus the displayed picture fit without increasing the item bound.
const DECODED_BUDGET: usize = 128 * 1024 * 1024;

/// Latest stream picture and whether the UI still has to upload it.
type Slot = Option<(platform::video::Picture, bool)>;

/// Replace the latest picture in place: a plain copy of the decoder's planes into one reused
/// allocation. Colour conversion happens on the GPU when the UI uploads it.
fn store_frame(slot: &mut Slot, frame: discord_voice::RemoteFrame<'_>) -> bool {
	let picture = frame.picture;
	if !picture.is_valid() {
		return false;
	}
	let len = picture.data.len();
	let mut data = slot.take().map(|(old, _)| old.data).unwrap_or_default();
	if data.capacity() > 1024 * 1024 && data.capacity() > len.saturating_mul(4) {
		// Release a previous large resolution while keeping ordinary resize reuse.
		data = Vec::new();
	}
	data.clear();
	// Exact growth: never double a previous resolution's allocation beyond the frame.
	data.reserve_exact(len);
	data.extend_from_slice(picture.data);
	*slot = Some((picture.with_data(data), true));
	true
}

// Decode early, display on the audio/device clock. Only decoded pictures may be
// skipped here: the H.264 decoder has already consumed their reference state.
#[derive(Default)]
struct Scheduled {
	latest: Slot,
	pending: std::collections::VecDeque<(platform::video::Picture, discord_voice::Presentation)>,
	bytes: usize,
}
impl Scheduled {
	fn store(&mut self, frame: discord_voice::RemoteFrame<'_>) -> bool {
		let Some(presentation) = frame.presentation.clone() else {
			return store_frame(&mut self.latest, frame);
		};
		if !presentation.is_current() || !frame.picture.is_valid() {
			return false;
		}
		let length = frame.picture.data.len();
		if length > DECODED_BUDGET {
			return false;
		}
		let old = if self.latest.as_ref().is_some_and(|(_, fresh)| !*fresh) {
			self.latest.take()
		} else {
			None
		};
		let mut data = old.map(|(picture, _)| picture.data).unwrap_or_default();
		if data.capacity() > length.saturating_mul(4) {
			data = Vec::new();
		}
		let required = data.capacity().max(length);
		while self.pending.len() >= 8
			|| self.bytes
				+ required + self
				.latest
				.as_ref()
				.map_or(0, |(picture, _)| picture.data.capacity())
				> DECODED_BUDGET
		{
			let Some((old, _)) = self.pending.pop_front() else {
				return false;
			};
			self.bytes -= old.data.capacity();
		}
		data.clear();
		data.reserve_exact(length);
		data.extend_from_slice(frame.picture.data);
		self.bytes += data.capacity();
		self.pending
			.push_back((frame.picture.with_data(data), presentation));
		true
	}
	fn advance(&mut self, now: Instant) -> Option<Instant> {
		while let Some((_, stamp)) = self.pending.front() {
			if stamp.is_current() && stamp.deadline(now) > now {
				return Some(stamp.deadline(now));
			}
			let (picture, stamp) = self.pending.pop_front().expect("front exists");
			self.bytes -= picture.data.capacity();
			if stamp.is_current() {
				self.latest = Some((picture, true));
			}
		}
		None
	}
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Context {
	generation: u64,
	channel: Id,
	request: u64,
	stream_request: u64,
	streamer: Id,
}
struct Pending {
	context: Context,
	user: Id,
	peer: Option<Id>,
	session: Zeroizing<String>,
	identity: Arc<Identity>,
	audio: Option<std::sync::mpsc::SyncSender<discord_voice::PlaybackFrame>>,
	rtc: Option<(Id, Id)>,
	server: Option<(voice::Secret, String)>,
	started: Instant,
}
#[derive(Clone, Copy)]
enum Notice {
	Status(&'static str),
	Failed(&'static str),
}
struct Live {
	context: Context,
	task: JoinHandle<()>,
	events: watch::Receiver<Option<Notice>>,
	frames: Arc<Mutex<Scheduled>>,
}

/// One watched stream's picture and status for the UI.
type View = (Option<egui::load::SizedTexture>, &'static str);

/// Every watched stream, in the order they were opened.
#[derive(Default)]
pub(super) struct Watch {
	streams: Vec<(Id, Single)>,
	/// Shared so each stream attempt gets a request id unique across streams.
	sequence: u64,
	/// Commands produced in one poll beyond the first; returned on the next polls.
	queued: std::collections::VecDeque<Command>,
	mixer: Option<Mixer>,
}
impl Watch {
	pub fn stop(&mut self) {
		for (_, single) in &mut self.streams {
			single.stop();
		}
		self.streams.clear();
		self.queued.clear();
		self.mixer = None;
	}
	/// Take negotiation secrets before the UI reduces the event. Nothing is persisted.
	pub fn observe(&mut self, state: &State, event: &mut Event) {
		for (_, single) in &mut self.streams {
			single.observe(state, event);
		}
	}
	pub fn poll(
		&mut self,
		runtime: &Runtime,
		state: &mut State,
		ui: &mut ui::MessagingUi,
		ctx: &egui::Context,
		call: Option<super::screen::Call<'_>>,
		audio: Option<std::sync::mpsc::SyncSender<discord_voice::PlaybackFrame>>,
	) -> Option<Command> {
		let wanted: Vec<(u64, Id, u64, Id)> = state
			.voice
			.active
			.as_ref()
			.filter(|active| active.phase != voice::Phase::Failed)
			.map(|active| {
				active
					.watching
					.iter()
					.map(|streamer| (state.generation, active.channel, active.request, *streamer))
					.collect()
			})
			.unwrap_or_default();
		for (_, _, _, streamer) in &wanted {
			if !self.streams.iter().any(|(id, _)| id == streamer) {
				self.streams.push((*streamer, Single::default()));
			}
		}
		// The mixer exists while the call offers a stream-audio input.
		match &audio {
			Some(out) if self.mixer.is_none() => self.mixer = Some(Mixer::start(out.clone())),
			None => self.mixer = None,
			_ => {}
		}
		let mut views = Vec::with_capacity(self.streams.len());
		for (streamer, single) in &mut self.streams {
			let target = wanted.iter().copied().find(|w| w.3 == *streamer);
			let input = self.mixer.as_mut().map(|mixer| mixer.input(*streamer));
			let mut view: View = (None, "");
			if let Some(command) = single.poll(
				runtime,
				state,
				ui,
				ctx,
				call.clone(),
				input,
				target,
				&mut self.sequence,
				&mut view,
			) {
				self.queued.push_back(command);
			}
			if target.is_some() {
				views.push((*streamer, view.0, view.1));
			}
		}
		self.streams
			.retain(|(streamer, single)| wanted.iter().any(|w| w.3 == *streamer) || single.busy());
		if let Some(mixer) = &mut self.mixer {
			mixer.retain(|streamer| self.streams.iter().any(|(id, _)| *id == streamer));
		}
		// The first stream also fills the single-stream fields older UI paths read.
		ui.voice_stream_view = views.first().and_then(|view| view.1);
		ui.voice_stream_status = views.first().map_or("", |view| view.2);
		ui.voice_streams = views;
		if self.queued.len() > 1 {
			ctx.request_repaint();
		}
		self.queued.pop_front()
	}
}

/// Each watched stream's audio input, shared with the mixing thread.
type MixInputs = Arc<Mutex<Vec<(Id, std::sync::mpsc::Receiver<discord_voice::PlaybackFrame>)>>>;

/// Routes watched streams' audio into the call's single stream-audio input.
///
/// With one stream playing, each frame is forwarded untouched and at once, so the
/// presentation timing that keeps sound and picture in sync is preserved. With several, one
/// frame from each playing stream is summed into the first stream's frame, which keeps that
/// frame's presentation timing.
struct Mixer {
	inputs: MixInputs,
	senders: Vec<(
		Id,
		std::sync::mpsc::SyncSender<discord_voice::PlaybackFrame>,
	)>,
	stop: Arc<std::sync::atomic::AtomicBool>,
}
impl Mixer {
	fn start(out: std::sync::mpsc::SyncSender<discord_voice::PlaybackFrame>) -> Self {
		let inputs: MixInputs = Arc::default();
		let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
		let (thread_inputs, thread_stop) = (inputs.clone(), stop.clone());
		let _ = std::thread::Builder::new()
			.name("serein-stream-mix".into())
			.spawn(move || {
				// A stream that sent nothing for this long no longer holds the others back.
				const IDLE: Duration = Duration::from_millis(120);
				struct Queue {
					id: Id,
					frames: std::collections::VecDeque<discord_voice::PlaybackFrame>,
					last: Instant,
				}
				let mut queues: Vec<Queue> = Vec::new();
				while !thread_stop.load(std::sync::atomic::Ordering::Acquire) {
					let now = Instant::now();
					if let Ok(inputs) = thread_inputs.lock() {
						queues.retain(|queue| inputs.iter().any(|(id, _)| *id == queue.id));
						for (id, receiver) in inputs.iter() {
							let index = match queues.iter().position(|queue| queue.id == *id) {
								Some(index) => index,
								None => {
									queues.push(Queue {
										id: *id,
										frames: Default::default(),
										last: now,
									});
									queues.len() - 1
								}
							};
							let queue = &mut queues[index];
							while let Ok(frame) = receiver.try_recv() {
								if queue.frames.len() >= 8 {
									queue.frames.pop_front();
								}
								queue.frames.push_back(frame);
								queue.last = now;
							}
						}
					}
					loop {
						let playing: Vec<usize> = (0..queues.len())
							.filter(|index| now.duration_since(queues[*index].last) < IDLE)
							.collect();
						// Mix only once every playing stream has a frame (or one stream plays).
						if playing.is_empty()
							|| playing.iter().any(|index| queues[*index].frames.is_empty())
						{
							break;
						}
						let mut frames = playing
							.iter()
							.filter_map(|index| queues[*index].frames.pop_front());
						let Some(mut mixed) = frames.next() else {
							break;
						};
						let mut summed = false;
						for frame in frames {
							summed = true;
							for (out, sample) in mixed.samples.iter_mut().zip(frame.samples.iter())
							{
								*out += sample;
							}
						}
						if summed {
							for sample in &mut mixed.samples {
								*sample = sample.clamp(-1.0, 1.0);
							}
						}
						if matches!(
							out.try_send(mixed),
							Err(std::sync::mpsc::TrySendError::Disconnected(_))
						) {
							return;
						}
					}
					// Idle queues keep only recent audio so a returning stream starts in sync.
					for queue in &mut queues {
						if now.duration_since(queue.last) >= IDLE {
							queue.frames.clear();
						}
					}
					std::thread::sleep(Duration::from_millis(2));
				}
			});
		Self {
			inputs,
			senders: Vec::new(),
			stop,
		}
	}
	/// This stream's audio sender, created on first use.
	fn input(&mut self, streamer: Id) -> std::sync::mpsc::SyncSender<discord_voice::PlaybackFrame> {
		if let Some((_, sender)) = self.senders.iter().find(|(id, _)| *id == streamer) {
			return sender.clone();
		}
		let (sender, receiver) = std::sync::mpsc::sync_channel(8);
		if let Ok(mut inputs) = self.inputs.lock() {
			inputs.push((streamer, receiver));
		}
		self.senders.push((streamer, sender.clone()));
		sender
	}
	fn retain(&mut self, keep: impl Fn(Id) -> bool) {
		self.senders.retain(|(id, _)| keep(*id));
		if let Ok(mut inputs) = self.inputs.lock() {
			inputs.retain(|(id, _)| keep(*id));
		}
	}
}
impl Drop for Mixer {
	fn drop(&mut self) {
		self.stop.store(true, std::sync::atomic::Ordering::Release);
	}
}

/// One watched stream's negotiation, media task and texture.
#[derive(Default)]
struct Single {
	pending: Option<Pending>,
	live: Option<Live>,
	/// The streamer stopped or Discord failed the view; the state choice is cleared next poll.
	ended: Option<(ui::design::Level, &'static str)>,
	status: &'static str,
	/// GPU texture behind this stream's view.
	texture: crate::gpu_video::VideoTexture,
}
impl Single {
	/// Still negotiating or connected, so it must be stopped explicitly.
	fn busy(&self) -> bool {
		self.pending.is_some() || self.live.is_some() || self.ended.is_some()
	}
	pub fn stop(&mut self) {
		self.pending = None;
		self.ended = None;
		if let Some(live) = self.live.take() {
			live.task.abort();
		}
		// Logout stops polling, so release the GPU picture here; disposal waits a frame.
		self.texture = Default::default();
	}
	fn context(&self) -> Option<Context> {
		self.pending
			.as_ref()
			.map(|pending| pending.context)
			.or_else(|| self.live.as_ref().map(|live| live.context))
	}
	fn observe(&mut self, state: &State, event: &mut Event) {
		let Event::Voice(voice::Event::Watch {
			channel,
			request,
			stream_request,
			streamer,
			event,
		}) = event
		else {
			return;
		};
		let Some(context) = self.context() else {
			return;
		};
		if context.generation != state.generation
			|| (
				context.channel,
				context.request,
				context.stream_request,
				context.streamer,
			) != (*channel, *request, *stream_request, *streamer)
		{
			return;
		}
		match event {
			screen::Event::Created {
				rtc_server,
				rtc_channel,
			} => {
				if let Some(pending) = &mut self.pending {
					pending.rtc = Some((*rtc_server, *rtc_channel));
				}
			}
			screen::Event::Server { token, endpoint } => {
				if let Some(pending) = &mut self.pending {
					match (token.take(), endpoint.take()) {
						(Some(token), Some(endpoint)) => pending.server = Some((token, endpoint)),
						// A null endpoint means Discord is still allocating the stream server.
						(_, None) => pending.server = None,
						(None, Some(_)) => {
							self.ended = Some((
								ui::design::Level::Warning,
								"Discord omitted the stream connection token",
							));
						}
					}
				}
			}
			screen::Event::Deleted { reason } => {
				self.ended = Some((
					ui::design::Level::Info,
					reason.unwrap_or("The stream ended"),
				));
			}
			screen::Event::Failed(message) => {
				self.ended = Some((ui::design::Level::Warning, message));
			}
			screen::Event::Viewers(_) => {}
		}
	}
	#[allow(clippy::too_many_arguments)]
	fn poll(
		&mut self,
		runtime: &Runtime,
		state: &mut State,
		ui: &mut ui::MessagingUi,
		ctx: &egui::Context,
		call: Option<super::screen::Call<'_>>,
		audio: Option<std::sync::mpsc::SyncSender<discord_voice::PlaybackFrame>>,
		// The separate stream transport survives call rekeys and voice resumption.
		wanted: Option<(u64, Id, u64, Id)>,
		sequence: &mut u64,
		view: &mut View,
	) -> Option<Command> {
		let current = self.context();
		let mut command = None;
		if let Some(context) = current
			&& (state.demo
				|| wanted
					!= Some((
						context.generation,
						context.channel,
						context.request,
						context.streamer,
					))) {
			self.stop();
			self.status = "Stopped watching";
			if context.generation == state.generation && !state.demo {
				command = Some(Command::Voice(voice::Command::StopWatching {
					channel: context.channel,
					request: context.request,
					stream_request: context.stream_request,
				}));
			}
		}
		if let Some((level, message)) = self.ended.take() {
			let context = self.context();
			self.stop();
			self.status = message;
			// Said once; the stage keeps no line for a view that already ended.
			ui.toasts.push(level, message);
			if let Some(streamer) = context
				.map(|context| context.streamer)
				.or(wanted.map(|wanted| wanted.3))
			{
				state.stop_watching_stream(streamer);
			}
			*view = (None, self.status);
			self.texture = Default::default();
			return command.or_else(|| {
				let context = context.filter(|context| context.generation == state.generation)?;
				Some(Command::Voice(voice::Command::StopWatching {
					channel: context.channel,
					request: context.request,
					stream_request: context.stream_request,
				}))
			});
		}
		if state.demo {
			view.1 = if wanted.is_some() {
				"Offline preview · no stream is received"
			} else {
				""
			};
			return command;
		}
		if let Some((generation, channel, request, streamer)) = wanted
			&& self.context().is_none()
			&& command.is_none()
		{
			let Some(call) = call.filter(|call| {
				call.generation == generation && call.channel == channel && call.request == request
			}) else {
				view.1 = "Connect the call before watching a stream";
				return None;
			};
			*sequence = sequence.wrapping_add(1);
			let context = Context {
				generation,
				channel,
				request,
				stream_request: *sequence,
				streamer,
			};
			self.pending = Some(Pending {
				context,
				user: call.user,
				peer: call.peer,
				session: Zeroizing::new(call.session.to_owned()),
				identity: call.identity,
				audio,
				rtc: None,
				server: None,
				started: Instant::now(),
			});
			self.status = "Requesting the stream…";
			command = Some(Command::Voice(voice::Command::WatchStream {
				channel,
				request,
				stream_request: context.stream_request,
				streamer,
			}));
		}
		if let Some(pending) = &self.pending {
			if pending.started.elapsed() >= SIGNAL_TIMEOUT {
				self.ended = Some((
					ui::design::Level::Warning,
					"Discord did not provide the stream connection; try again",
				));
				ctx.request_repaint();
			} else {
				ctx.request_repaint_after(SIGNAL_TIMEOUT.saturating_sub(pending.started.elapsed()));
			}
		}
		if self
			.pending
			.as_ref()
			.is_some_and(|pending| pending.rtc.is_some() && pending.server.is_some())
		{
			let pending = self.pending.take().expect("complete stream negotiation");
			if let Err(error) = self.start(runtime, pending, ctx) {
				self.ended = Some((ui::design::Level::Warning, error));
			}
		}
		if let Some(live) = &mut self.live {
			match *live.events.borrow_and_update() {
				Some(Notice::Status(status)) => self.status = status,
				Some(Notice::Failed(error)) => {
					self.ended = Some((ui::design::Level::Warning, error));
				}
				None => {}
			}
			if live.task.is_finished() && self.ended.is_none() {
				self.ended = Some((ui::design::Level::Warning, "The stream connection ended"));
			}
			// Uploading copies the planes into GPU staging, so it runs under the slot lock
			// and the decoder keeps refilling the same allocation.
			if let Ok(mut slot) = live.frames.try_lock() {
				if let Some(deadline) = slot.advance(Instant::now()) {
					ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
				}
				if let Some((picture, fresh)) = slot.latest.as_mut()
					&& std::mem::take(fresh)
					&& let Some(texture) =
						self.texture.upload(ctx, "remote-stream", picture.borrow())
				{
					self.status = "Watching the stream";
					view.0 = Some(texture);
				} else {
					view.0 = self.texture.texture();
				}
			} else {
				view.0 = self.texture.texture();
			}
		} else {
			self.texture = Default::default();
		}
		view.1 = if wanted.is_some() { self.status } else { "" };
		command
	}
	fn start(
		&mut self,
		runtime: &Runtime,
		mut pending: Pending,
		ctx: &egui::Context,
	) -> Result<(), &'static str> {
		let (rtc_server, rtc_channel) = pending.rtc.take().ok_or("Missing stream RTC identity")?;
		let (token, endpoint) = pending.server.take().ok_or("Missing stream server")?;
		let session = voice::Secret::new(pending.session.to_string())
			.map_err(|_| "Invalid stream voice session")?;
		let credentials = voice::VoiceConnection {
			channel: rtc_channel,
			guild: Some(rtc_server),
			user: pending.user,
			peer: pending.peer,
			session,
			token,
			endpoint,
			request: pending.context.stream_request,
		};
		let frames = Arc::new(Mutex::new(Scheduled::default()));
		let slot = frames.clone();
		let wake = ctx.clone();
		let streamer = pending.context.streamer.0;
		let sink: discord_voice::VideoSink = Arc::new(move |frame: discord_voice::RemoteFrame| {
			if frame.user != streamer {
				return;
			}
			if slot.lock().is_ok_and(|mut slot| slot.store(frame)) {
				wake.request_repaint();
			}
		});
		let (send, events) = watch::channel(None);
		let wake = ctx.clone();
		let identity = pending.identity;
		let audio = pending.audio;
		let task = runtime.spawn(async move {
			let (status_send, status_wake) = (send.clone(), wake.clone());
			let result =
				discord_voice::watch_stream(credentials, identity, sink, audio, move |event| {
					let status = match event {
						Status::Connecting => "Connecting to the stream…",
						Status::Discovering => "Checking the stream network…",
						Status::TransportReady | Status::Securing => "Securing the stream…",
						Status::WaitingForPeer => "Waiting for the streamer…",
						Status::Ready { .. } => "Stream secured · waiting for video",
						Status::RemoteAudio | Status::Speaking(_) | Status::CameraAvailable(_) => {
							return Ok(());
						}
					};
					status_send.send_replace(Some(Notice::Status(status)));
					status_wake.request_repaint();
					Ok(())
				})
				.await;
			if let Err(error) = result {
				send.send_replace(Some(Notice::Failed(error)));
			}
			wake.request_repaint();
		});
		self.status = "Connecting to the stream…";
		self.live = Some(Live {
			context: pending.context,
			task,
			events,
			frames,
		});
		Ok(())
	}
}
impl Drop for Single {
	fn drop(&mut self) {
		self.stop();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn watch_survives_call_recovery_until_its_selection_ends() {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let mut state = test_support::voice_demo_state();
		state.demo = false;
		state.watch_stream(Id(3)).unwrap();
		let call = state.voice.active.as_ref().unwrap();
		let context = Context {
			generation: state.generation,
			channel: call.channel,
			request: call.request,
			stream_request: 2,
			streamer: Id(3),
		};
		let mut ui = ui::MessagingUi::default();
		let ctx = egui::Context::default();
		for failed in [false, true] {
			state.voice.active.as_mut().unwrap().watching = vec![context.streamer];
			let (_send, events) = watch::channel(None);
			let mut watch = Watch::default();
			watch.streams.push((
				context.streamer,
				Single {
					pending: None,
					live: Some(Live {
						context,
						task: runtime.spawn(std::future::pending()),
						events,
						frames: Arc::new(Mutex::new(Scheduled::default())),
					}),
					ended: None,
					status: "",
					texture: Default::default(),
				},
			));
			for phase in [
				voice::Phase::Connected,
				voice::Phase::Securing,
				voice::Phase::ConnectingTransport,
				voice::Phase::OpeningAudio,
				voice::Phase::Waiting,
				voice::Phase::Connected,
			] {
				state.voice.active.as_mut().unwrap().phase = phase;
				assert!(
					watch
						.poll(&runtime, &mut state, &mut ui, &ctx, None, None)
						.is_none()
				);
				assert!(watch.streams[0].1.live.is_some());
			}
			if failed {
				state.voice.active.as_mut().unwrap().phase = voice::Phase::Failed;
			} else {
				state.stop_watching();
			}
			assert!(matches!(
				watch.poll(&runtime, &mut state, &mut ui, &ctx, None, None),
				Some(Command::Voice(voice::Command::StopWatching {
					channel, request, stream_request: 2,
				})) if channel == context.channel && request == context.request
			));
			assert!(watch.streams.is_empty());
		}
	}

	fn frame(width: u32, height: u32, data: &[u8]) -> discord_voice::RemoteFrame<'_> {
		discord_voice::RemoteFrame {
			presentation: None,
			user: 7,
			picture: platform::video::Picture {
				width,
				height,
				layout: platform::video::Layout::I420,
				color: platform::video::Color::WEBRTC,
				rotation: 0,
				data,
			},
		}
	}

	fn i420(width: u32, height: u32, value: u8) -> Vec<u8> {
		vec![value; platform::video::Layout::I420.len(width, height).unwrap()]
	}

	#[test]
	fn watch_reuses_the_frame_buffer_and_flags_only_updates() {
		let mut slot = None;
		assert!(store_frame(&mut slot, frame(2, 2, &i420(2, 2, 1))));
		let planes = slot.as_ref().unwrap().0.data.as_ptr();
		for value in 0..20 {
			slot.as_mut().unwrap().1 = false;
			assert!(store_frame(&mut slot, frame(2, 2, &i420(2, 2, value))));
			let (picture, fresh) = slot.as_ref().unwrap();
			assert!(*fresh);
			assert_eq!(picture.data.as_ptr(), planes);
			assert_eq!(picture.data, i420(2, 2, value));
		}
		// A smaller picture reuses the allocation too.
		assert!(store_frame(&mut slot, frame(1, 1, &i420(1, 1, 9))));
		let (picture, _) = slot.as_ref().unwrap();
		assert_eq!(picture.data.as_ptr(), planes);
		assert_eq!((picture.width, picture.height), (1, 1));
	}

	#[test]
	fn watch_invalid_frames_preserve_the_last_valid_picture() {
		let mut slot = None;
		assert!(store_frame(&mut slot, frame(1, 1, &i420(1, 1, 4))));
		let planes = slot.as_ref().unwrap().0.data.as_ptr();
		for invalid in [
			frame(0, 1, &[]),
			frame(1, 0, &[]),
			frame(1, 1, &[0; 2]),
			frame(u32::MAX, u32::MAX, &[]),
		] {
			assert!(!store_frame(&mut slot, invalid));
			let (picture, _) = slot.as_ref().unwrap();
			assert_eq!(picture.data.as_ptr(), planes);
			assert_eq!((picture.width, picture.height), (1, 1));
		}
	}

	#[test]
	fn watch_releases_large_capacity_after_a_resolution_drop() {
		let mut slot = None;
		assert!(store_frame(
			&mut slot,
			frame(1920, 1080, &i420(1920, 1080, 0))
		));
		assert!(store_frame(&mut slot, frame(16, 16, &i420(16, 16, 0))));
		let (picture, _) = slot.take().unwrap();
		assert_eq!(picture.data.len(), 16 * 16 * 3 / 2);
		assert!(picture.data.capacity() <= 16 * 16 * 6);
	}

	#[test]
	fn watch_resolution_growth_does_not_double_the_capacity() {
		let mut slot = None;
		assert!(store_frame(
			&mut slot,
			frame(1600, 900, &i420(1600, 900, 0))
		));
		assert!(store_frame(
			&mut slot,
			frame(1920, 1080, &i420(1920, 1080, 0))
		));
		let (picture, _) = slot.take().unwrap();
		assert_eq!(picture.data.len(), 1920 * 1080 * 3 / 2);
		assert!(picture.data.capacity() <= 1920 * 1080 * 3 / 2);
	}
	#[test]
	fn decoded_stream_pictures_wait_for_presentation_and_pressure_skips_only_decoded_frames() {
		let now = Instant::now();
		let mut queue = Scheduled::default();
		let pixels = i420(3840, 2160, 4);
		for n in 0..16 {
			let mut picture = frame(3840, 2160, &pixels);
			picture.presentation = Some(discord_voice::Presentation::new(
				now + Duration::from_millis(100 + n * 16),
			));
			assert!(queue.store(picture));
		}
		assert_eq!(queue.pending.len(), 8);
		assert!(queue.bytes <= DECODED_BUDGET);
		assert!(queue.latest.is_none());
		assert_eq!(queue.advance(now), Some(now + Duration::from_millis(228)));
		queue.advance(now + Duration::from_millis(260));
		assert_eq!(queue.pending.len(), 5);
		assert!(queue.latest.as_ref().unwrap().1);
		assert!(queue.bytes + queue.latest.as_ref().unwrap().0.data.capacity() <= DECODED_BUDGET);
		drop(queue);
		// A backend's RGBA fallback reaches the byte bound before the eight-picture bound.
		let mut queue = Scheduled::default();
		let pixels = vec![4; 3840 * 2160 * 4];
		for n in 0..16 {
			let mut picture = frame(3840, 2160, &pixels);
			picture.picture.layout = platform::video::Layout::Rgba;
			picture.presentation = Some(discord_voice::Presentation::new(
				now + Duration::from_millis(100 + n * 16),
			));
			assert!(queue.store(picture));
		}
		assert_eq!(queue.pending.len(), 4);
		assert!(queue.bytes <= DECODED_BUDGET);
		queue.advance(now + Duration::from_secs(1));
		assert!(queue.pending.is_empty());
		assert_eq!(queue.bytes, 0);
		assert!(queue.latest.as_ref().unwrap().1);
	}
}
