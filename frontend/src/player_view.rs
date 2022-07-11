use crate::{
    caching,
    queue_view::{CachedArticle, CachedArticleHandle},
    WeakComponentLink,
};

use serde::{Deserialize, Serialize};
use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    Blob, BlobPropertyBag, HtmlAudioElement, HtmlSelectElement, MediaMetadata, MediaPositionState,
    MediaSession, MediaSessionAction, MediaSessionActionDetails, Url,
};
use yew::{html::Scope, prelude::*};

const PLAYER_ID: &str = "player";
const SPEED_SELECTOR_ID: &str = "speed-selector";
const AUDIO_MIME_FORMAT: &str = "audio/mp3";

// The number of milliseconds between times saving Player state
const PLAYER_STATE_SAVE_FREQ: i32 = 10000;

// Always jump by 10sec
const JUMP_SIZE: f64 = 10.0;

/// Helper function to retrieve the only audio element from the page
fn get_audio_elem() -> HtmlAudioElement {
    gloo_utils::document()
        .get_element_by_id(PLAYER_ID)
        .unwrap()
        .dyn_into()
        .unwrap()
}

/// Helper function to retrieve the MediaSession API
fn get_media_session() -> MediaSession {
    gloo_utils::window().navigator().media_session()
}

/// Sets the current playback time to the given time
fn set_current_time(time: f64) {
    let audio_elem = get_audio_elem();
    audio_elem.set_current_time(time);
}

/// Sets the playback speed
fn set_playback_speed(speed: f64) {
    let audio_elem = get_audio_elem();
    audio_elem.set_playback_rate(speed);
}

/// Jumps forward or backwards by the specified offset
fn jump_offset(audio_elem: &HtmlAudioElement, offset: f64) {
    // New time must be in the range [0, duration]
    let new_time = f64::min(audio_elem.duration(), audio_elem.current_time() + offset);
    let new_time = f64::max(0.0, new_time);

    audio_elem.set_current_time(new_time);
}

/// Updates the MediaSession's scrubber to the current elapsed track time
fn update_playback_state() {
    let audio_elem = get_audio_elem();

    // Get the current position, duration, and playback rate from the <audio> element
    let pos = audio_elem.current_time();
    let dur = audio_elem.duration();
    let rate = audio_elem.playback_rate();

    // If any of the above values are not in the range [0, ∞), then the player is not configured.
    // Do not set anything, lest an panic occur
    if ![pos, dur, rate]
        .into_iter()
        .all(|x| x.is_finite() && x >= 0.0)
    {
        return;
    }

    // Update the position state
    let mut playback_state = MediaPositionState::new();
    playback_state
        .position(pos)
        .duration(dur)
        .playback_rate(rate);

    // Now give the above metadata to the media session
    let media_session = get_media_session();
    media_session.set_position_state_with_state(&playback_state);

    tracing::debug!("Updated MediaSession state");
}

// TODO: use wasm_bindgen generated getters to get fields from these dicts. This is blocked on
// https://github.com/rustwasm/wasm-bindgen/issues/2921
/// Callback for the "seekto" MediaSession action
fn seek_to(evt: MediaSessionActionDetails) {
    let audio_elem = get_audio_elem();

    let fast_seek = js_sys::Reflect::get(&evt, &JsValue::from_str("fastSeek")).map(|t| t.as_bool());
    let seek_time = js_sys::Reflect::get(&evt, &JsValue::from_str("seekTime")).map(|t| t.as_f64());
    let seek_offset =
        js_sys::Reflect::get(&evt, &JsValue::from_str("seekOffset")).map(|t| t.as_f64());

    tracing::debug!(
        "Seeking to offset {:?} or time {:?}",
        seek_offset,
        seek_time
    );

    // Seek to the specified time, if defined
    if let Ok(Some(time)) = seek_time {
        // If "fast seek" is set, us that method
        match fast_seek {
            Ok(Some(true)) => audio_elem.fast_seek(time).unwrap(),
            _ => audio_elem.set_current_time(time),
        }
    } else if let Ok(Some(off)) = seek_offset {
        jump_offset(&audio_elem, off);
    }
}

/// Jumps forward by JUMP_SIZE seconds
fn jump_forward() {
    tracing::debug!("Jumping forward",);
    let audio_elem = get_audio_elem();
    jump_offset(&audio_elem, JUMP_SIZE);
    update_playback_state();
}

/// Jumps backward by JUMP_SIZE seconds
fn jump_backward() {
    tracing::debug!("Jumping backward",);
    let audio_elem = get_audio_elem();
    jump_offset(&audio_elem, -JUMP_SIZE);
    update_playback_state();
}

fn set_callbacks(media_session: &MediaSession, actions: &Actions) {
    // Helper function for annoying conversion from Closure to Function
    fn action_to_func_ref<T: ?Sized>(action: &Option<Closure<T>>) -> &js_sys::Function {
        action.as_ref().unwrap().as_ref().unchecked_ref()
    }

    media_session.set_action_handler(
        MediaSessionAction::Play,
        Some(action_to_func_ref(&actions.play_action)),
    );
    media_session.set_action_handler(
        MediaSessionAction::Pause,
        Some(action_to_func_ref(&actions.pause_action)),
    );
    media_session.set_action_handler(
        MediaSessionAction::Seekforward,
        Some(action_to_func_ref(&actions.jump_forward_action)),
    );
    media_session.set_action_handler(
        MediaSessionAction::Seekbackward,
        Some(action_to_func_ref(&actions.jump_backward_action)),
    );
    media_session.set_action_handler(
        MediaSessionAction::Seekto,
        Some(action_to_func_ref(&actions.seek_to_action)),
    );
}

fn play() {
    let audio_elem = get_audio_elem();
    spawn_local(async move {
        let promise = audio_elem.play().unwrap();
        let res = JsFuture::from(promise).await;
        if let Err(e) = res {
            tracing::error!("Error playing track: {:?}", e);
        }
    });
}

fn pause() {
    let audio_elem = get_audio_elem();
    audio_elem.pause().unwrap();
}

fn set_audio_source(art: &CachedArticle) {
    // Pause the current
    let audio_elem = get_audio_elem();
    audio_elem.pause().unwrap();

    // Make a blob from the MP3 bytes
    let blob = {
        let bytes = js_sys::Uint8Array::from(art.audio_blob.as_slice());

        // A blob is made from an array of arrays. So construct [bytes] and use that.
        let parts = js_sys::Array::new();
        parts.set(0, JsValue::from(bytes));
        Blob::new_with_u8_array_sequence_and_options(
            &parts,
            BlobPropertyBag::new().type_(AUDIO_MIME_FORMAT),
        )
        .unwrap()
    };

    // Initialize the MediaSession with metadata and callbacks
    let metadata = MediaMetadata::new().unwrap();
    metadata.set_title(&art.title);
    let media_session = get_media_session();
    media_session.set_metadata(Some(&metadata));

    // Construct a URL that refers to the blob. This will be the audio player's src attribute
    let blob_url = Url::create_object_url_with_blob(&blob).unwrap();

    // Now play the audio
    audio_elem.set_src(&blob_url);
}

fn play_article(art: &CachedArticle) {
    set_audio_source(art);
    play();
}

fn set_audio_source_by_handle(handle: &CachedArticleHandle) {
    // Load the article and play it
    let handle = handle.clone();
    spawn_local(async move {
        match caching::load_article(&handle).await {
            Ok(article) => set_audio_source(&article),
            Err(e) => {
                tracing::error!("Couldn't load article {}: {:?}", handle.0, e);
                return;
            }
        };
    })
}

fn play_article_handle(handle: &CachedArticleHandle) {
    // Do a useless pause() action. This necessary because Safari is buggy and doesn't allow the
    // first media action (like play or pause) to come from inside an async worker
    pause();

    // Load the article and play it
    let handle = handle.clone();
    spawn_local(async move {
        match caching::load_article(&handle).await {
            Ok(article) => play_article(&article),
            Err(e) => {
                tracing::error!("Couldn't load article {}: {:?}", handle.0, e);
                return;
            }
        };
    })
}

/// Fetches the selected playback speed, updates the audio element accordingly, and returns the
/// selected speed
fn update_playback_speed() -> f64 {
    // Get the selected playback rate. If it's not a number, treat it as 1x speed
    let speed_selector: HtmlSelectElement = gloo_utils::document()
        .get_element_by_id(SPEED_SELECTOR_ID)
        .unwrap()
        .dyn_into()
        .unwrap();
    let rate: f64 = speed_selector.value().parse().unwrap_or(1.0);

    // Set the playback rate and update the MediaSession
    let audio_elem = get_audio_elem();
    audio_elem.set_playback_rate(rate);
    audio_elem.set_default_playback_rate(rate);
    update_playback_state();

    rate
}

/// Gets the elapsed time and tells the player to save its state (wrt the elapsed time and all the
/// player's other stored values)
fn trigger_save(player: &Scope<Player>) {
    let audio_elem = get_audio_elem();
    let elapsed = audio_elem.current_time();
    player.send_message(PlayerMsg::SaveState { elapsed });
}

#[derive(PartialEq, Properties)]
pub struct Props {
    /// A link to myself. We have to set this on creation
    pub player_link: WeakComponentLink<Player>,
}

pub enum PlayerMsg {
    /// Play the given article
    PlayHandle(CachedArticleHandle),

    /// Jump forward `JUMP_SIZE` seconds
    JumpForward,

    /// Jump backward `JUMP_SIZE` seconds
    JumpBackward,

    /// Triggers the Player to check the playback speed selector and update the playback speed
    /// accordingly
    UpdatePlaybackSpeed,

    /// Set the current state to the one provided. This is used for loading state from the
    /// IndexedDB
    SetState(PlayerState),

    // A message for the PlayerState to save itself. The only value that's stale is the elapsed
    // time, so that's given to the PlayerState
    SaveState {
        elapsed: f64,
    },
}

/// These are the callbacks the browser calls when the user performs a MediaSession operation like
/// seeking forward or skipping a track
#[derive(Default)]
struct Actions {
    play_action: Option<Closure<dyn 'static + Fn()>>,
    pause_action: Option<Closure<dyn 'static + Fn()>>,
    jump_forward_action: Option<Closure<dyn 'static + Fn()>>,
    jump_backward_action: Option<Closure<dyn 'static + Fn()>>,
    seek_to_action: Option<Closure<dyn 'static + Fn(MediaSessionActionDetails)>>,
}

/// The Player component of our app. This handles all the player logic.
pub struct Player {
    /// These are all the callbacks for MediaSession events like pause or jump forward. These need
    /// to live in the `Player` because otherwise they go out of scope
    _actions: Actions,
    /// The closure that runs every PLAYER_STATE_SAVE_FREQ seconds saving the player state
    _trigger_save_cb: Closure<dyn 'static + Fn()>,
    /// Holds all the serializable state of this player. This will be loaded from the IndexedDB
    state: PlayerState,
}

/// Holds what's playing, how long it's been playing, and how fast
#[derive(Clone, Serialize, Deserialize)]
pub struct PlayerState {
    /// Handle of the currently playing article
    now_playing: Option<CachedArticleHandle>,
    /// The elapsed time of the current article, in seconds
    elapsed: Option<f64>,
    /// The audio playback speed, as a percentage
    playback_speed: f64,
}

impl Default for PlayerState {
    fn default() -> PlayerState {
        PlayerState {
            now_playing: None,
            elapsed: None,
            playback_speed: 1.0,
        }
    }
}

/// Runs the given closure after `secs` seconds
fn run_after_delay(closure: &Closure<dyn 'static + Fn()>, secs: i32) {
    let win = gloo_utils::window();
    let func = closure.as_ref().unchecked_ref();
    if let Err(e) = win.set_timeout_with_callback_and_timeout_and_arguments_0(func, secs) {
        tracing::error!("Could not save player state: {:?}", e);
    }
}

/// Fetches the last saved player state and sets it as the current state
async fn build_from_save(player: &Scope<Player>) {
    if let Ok(state) = caching::get_player_state().await {
        tracing::info!("successfully restored player from save");
        player.send_message(PlayerMsg::SetState(state));
    }
}

impl Component for Player {
    type Message = PlayerMsg;
    type Properties = Props;

    fn update(&mut self, _ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            PlayerMsg::PlayHandle(handle) => {
                // Play the track and save it in now_playing
                tracing::debug!("Playing track {}", handle.0);
                play_article_handle(&handle);
                self.state.now_playing = Some(handle);

                false
            }
            PlayerMsg::JumpForward => {
                jump_forward();
                false
            }
            PlayerMsg::JumpBackward => {
                jump_backward();
                false
            }
            PlayerMsg::UpdatePlaybackSpeed => {
                // Check the playback speed selector and update the playback speed accordingly.
                // Also save the speed in the state.
                let rate = update_playback_speed();
                self.state.playback_speed = rate;

                false
            }
            PlayerMsg::SetState(state) => {
                // Set the state and start playing
                self.state = state;
                if let Some(handle) = &self.state.now_playing {
                    set_audio_source_by_handle(&handle);
                    set_current_time(self.state.elapsed.unwrap_or(0.0));
                    set_playback_speed(self.state.playback_speed);
                }

                // The state was updated. Refresh the player view
                true
            }
            PlayerMsg::SaveState { elapsed } => {
                // Update the elapsed time and save the state
                self.state.elapsed = Some(elapsed);
                let state_copy = self.state.clone();
                spawn_local(async move {
                    match caching::save_player_state(&state_copy).await {
                        Ok(_) => tracing::trace!("Successfully saved player state"),
                        Err(e) => tracing::error!("Could not save player state: {:?}", e),
                    }
                });

                // Now setup the next trigger
                run_after_delay(&self._trigger_save_cb, PLAYER_STATE_SAVE_FREQ);

                false
            }
        }
    }

    fn create(ctx: &Context<Self>) -> Self {
        // Set the player link to this Player
        ctx.props()
            .player_link
            .borrow_mut()
            .replace(ctx.link().clone());

        // Wrap the media session actions in closures so we can give them to the API
        let actions = Actions {
            play_action: Some(Closure::new(play)),
            pause_action: Some(Closure::new(pause)),
            jump_forward_action: Some(Closure::new(jump_forward)),
            jump_backward_action: Some(Closure::new(jump_backward)),
            seek_to_action: Some(Closure::new(seek_to)),
        };
        set_callbacks(&get_media_session(), &actions);

        // Set up the closure that gets called every 10sec and triggers a save event
        let link = ctx.link().clone();
        let trigger_save_cb = Closure::new(move || trigger_save(&link));

        // Kick off a future to get the last known player state
        let link = ctx.link().clone();
        spawn_local(async move { build_from_save(&link).await });

        // Kick off the state saving loop in PLAYER_STATE_SAVE_FREQ seconds
        run_after_delay(&trigger_save_cb, PLAYER_STATE_SAVE_FREQ);

        // Return the default values for now. Hopefully they get overwritten by the build_from_save
        // function
        Self {
            _actions: actions,
            _trigger_save_cb: trigger_save_cb,
            state: PlayerState::default(),
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        // Callbacks for the left and right arrow buttons
        let jump_forward_cb = ctx.link().callback(|_| PlayerMsg::JumpForward);
        let jump_backward_cb = ctx.link().callback(|_| PlayerMsg::JumpBackward);
        let playback_speed_cb = ctx.link().callback(|_| PlayerMsg::UpdatePlaybackSpeed);

        let now_playing_str = self
            .state
            .now_playing
            .as_ref()
            .map(|c| c.0.clone())
            .unwrap_or(String::default());

        html! {
            <section title="player">
                <p><b>{ "Now Playing: " }</b> { now_playing_str }</p>
                <audio controls=true style={ "display: block;" } id={PLAYER_ID}>
                    { "Your browser does not support the <code>audio</code> element" }
                </audio>
                <div class="audiocontrol" title="More playback controls">
                    <button title="Jump backwards 10 seconds" onclick={jump_backward_cb}>{ "↩️" }</button>
                    <button title="Jump forwards 10 seconds" onclick={jump_forward_cb}>{ "↪️" }</button>
                    <div class="playbackSpeedSection">
                        <label for={SPEED_SELECTOR_ID}>{ "Playback Speed:" }</label>
                        <select title="Playback speed" name={SPEED_SELECTOR_ID} id={SPEED_SELECTOR_ID} onchange={playback_speed_cb}>
                            <option value="0.5">{ "0.5" }</option>
                            <option value="0.75">{ "0.75" }</option>
                            <option value="1" selected=true>{ "1" }</option>
                            <option value="1.25">{ "1.25" }</option>
                            <option value="1.5">{ "1.5" }</option>
                            <option value="1.75">{ "1.75" }</option>
                            <option value="2">{ "2" }</option>
                            <option value="2.5">{ "2.5" }</option>
                            <option value="3">{ "3" }</option>
                            <option value="4">{ "4" }</option>
                        </select>
                    </div>
                </div>
            </section>
        }
    }
}
