//! What the behaviour assumes about Windows, players and speakers until
//! someone has tried it on a PC with real speakers. The constants record
//! those assumptions. Some are read by the rules, others describe trials
//! whose results have yet to be implemented.

/// Outputs. Windows' notices of an output coming or going and of its
/// default changing may arrive in either order, so a change of the default
/// within the window of an output coming or going, before or after it,
/// counts as Windows' doing.
pub const T1_OUTPUT_CHANGE_EITHER_SIDE: bool = true;

/// Outputs. Windows may keep to the app's device when headphones it
/// knew before are connected; the app then switches to them itself.
pub const T1_APP_SWITCHES_TO_HEADPHONES: bool = true;

/// Players. After a pause pressed on the speaker was passed to a player,
/// the app can't tell its own request from a person's except by what it
/// asked for: any change it didn't ask for counts as the person touching
/// the player, and play pressed on the speaker no longer starts it.
pub const T2_ANY_UNASKED_CHANGE_IS_A_TOUCH: bool = true;

/// Players. Players are matched to what was sounding by their app
/// (the program), not by their process, so that an app of several
/// processes is matched. A player whose app wasn't sounding is left alone.
pub const T2_MATCH_PLAYERS_BY_APP: bool = true;

/// Sonos and other UPnP speakers. Play pressed on the speaker itself is
/// not counted on to start the players that the speaker's own pause
/// stopped.
pub const T3_PLAY_ON_SPEAKER_STARTS_PLAYER: bool = false;

/// UPnP speakers that aren't a Sonos (a TV, a receiver) are taken to report
/// pauses, stops and another source as a Sonos does, and to let the app take
/// its stream off them.
pub const T14_OTHER_UPNP_LIKE_SONOS: bool = true;

/// AirPlay. A speaker that can't show what it is doing and ends this
/// PC's session while it can still be reached counts as someone else
/// playing there, without tries.
pub const T4_REACHABLE_END_IS_SOMEONE_ELSE: bool = true;

/// Changing place. This PC gets the sound only once the speaker that
/// played it confirmed it stopped, or after this long without an answer.
pub const T5_SILENCE_WAIT_MS: u64 = 2_000;

/// AirPlay. Pause and play pressed on an AirPlay speaker aren't
/// listened for.
pub const T6_AIRPLAY_REMOTE_CONTROL: bool = false;

/// During the grace period after start or wake, a failed request or a
/// speaker disappearing from discovery does not mark it away. Afterwards
/// either observation can mark an idle speaker away.
pub const T7_AWAY_ONLY_AFTER_A_FAILED_REQUEST: bool = true;

/// Music Assistant. It isn't counted on to name the room it plays in.
pub const T8_MUSIC_ASSISTANT_NAMES_ROOM: bool = false;

/// Sound and outputs. While This PC is the place, what still arrives
/// at the app's device (from apps that didn't follow Windows' change) is
/// played on the PC's own output.
pub const T9_PASS_THROUGH: bool = true;

/// Calls. A microphone coming into use isn't seen: there is no note
/// about a call on the speaker. Before this is turned on: the panel's note
/// about a call stands in place of the delay controls for as long as the
/// call lasts and can't be closed. It needs a × that lasts for that call.
pub const T10_MICROPHONE_SEEN: bool = false;

/// Networks. A VPN, or a second connection to the same network,
/// isn't another network; the firewall's rule isn't read as a fact.
pub const T11_VPN_IS_NOT_ANOTHER_NETWORK: bool = true;

/// Proposed screen-reader trial: also deliver pop-ups as Windows
/// notifications. Production does not read this switch.
pub const T12_POPUPS_ALSO_AS_NOTIFICATIONS: bool = true;

/// Two people signed in at once. Nothing is assumed either way about
/// whether a second person's sound reaches this app's device: the app
/// behaves as if it were the only one.
pub const T15_ASSUMES_NOTHING_ABOUT_TWO_USERS: bool = true;
