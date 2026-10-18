# Notification audio protocol v1

The companion owns three user-selectable notification clips: slot 0 completion,
slot 1 needs input, slot 2 error. It decodes PCM16 WAV, mixes mono/stereo to mono,
and resamples to 16 kHz. Input is limited to three seconds and 4 MB. Each device
clip is at most 96,000 bytes (48,000 signed little-endian PCM16 samples).

## Transport

Commands use existing KM v1 Report 6 packets over either USB or BLE. Host packet
bytes 0..10 keep the existing framing; byte 0 is report ID 6. The 53-byte command
body begins at host byte 11 (firmware byte 10 after stripping the report ID).
Integer fields below are little endian. Unused body bytes are zero.

| Command | Number | Body |
| --- | --- | --- |
| AUDIO_CAPS | 24 | byte 0 = audio protocol version 1 |
| AUDIO_BEGIN | 25 | slot u8, PCM length u32, expected CRC32 u32 |
| AUDIO_DATA | 26 | slot u8, offset u32, count u8 (1..46), ACK-request u8 (0/1), sample bytes |
| AUDIO_COMMIT | 27 | slot u8, expected CRC32 u32 |
| AUDIO_PLAY | 28 | slot u8; 255 stops playback |
| AUDIO_CLEAR | 29 | slot u8 |

ACK uses the existing `command | 0x80`, sequence u16, status u8. Status 0 means
success, 1 invalid request/incomplete transfer/CRC mismatch, 4 allocation or
playback failure. Unsupported firmware returns its existing bad-command status.
CAPS version 1 must succeed before any audio upload. Allocation failure is
reported separately; CAPS does not promise available PSRAM.

DATA requests an ACK every eight packets and at the end of the clip. Errors
always send an ACK. The host paces packets at 4 ms and retries each acknowledged
batch up to three times, with a two-second ACK deadline per attempt. Previously
accepted bytes can be resent only when they match exactly. Forward gaps and
changed duplicates are rejected. CRC32 is IEEE (polynomial 0xedb88320, initial and
final XOR 0xffffffff). COMMIT retries with the same slot/CRC are idempotent.

## Ownership and recovery

The device allocates staging storage from PSRAM. Three full clips plus one full
staging buffer use at most 384,000 PCM bytes. A successful complete CRC check
atomically replaces the old slot. Failed/truncated transfers leave old clips
usable. Unfinished staging expires after ten seconds without accepted data;
a new BEGIN frees previous staging. Allocation is checked rather than assuming
PSRAM initialization succeeded. No SPIFFS or NVS writes occur.

Speaker playback copies small PCM blocks to its internal I2S buffers. A slot
being replaced/cleared is stopped before its old buffer is freed. Other slots
remain available while the new one is uploaded. Notification status edges map
to the cached slot ID; absent cached audio uses the existing SPIFFS WAV fallback.

The companion saves validated PCM and the display filename under its per-user
configuration directory, in `notification-audio/`. It keeps built-in defaults
inside the application. Connection and boot-ID changes recache all three clips;
settings changes send only clips whose CRC changed. Audio uses a dedicated worker
and ACK queue, leaving status/glyph traffic independent. The shared device lock
is released after every write; the read loop is paused only during each small
batch and is resumed before waiting for an ACK.

The Sounds page offers WAV selection, restore default, sync progress/retry, and
playback on the actual keyboard. Preview is available whenever the keyboard is
connected: a slot whose PCM is not cached yet plays the firmware's built-in
SPIFFS sound, so testing never requires choosing a WAV first. `GET
/api/notification-audio` is read-only runtime diagnostics. Updates and previews
use the native Tauri commands.

## Verification

Host: `cargo test --manifest-path src-tauri/Cargo.toml --lib notification_audio`.
Device portable tests (run in firmware root):

```sh
c++ -std=c++17 -Wall -Wextra -Werror -fsanitize=address,undefined main/tools/notification_audio_test.cpp -o /tmp/notification_audio_test
/tmp/notification_audio_test
c++ -std=c++17 -Wall -Wextra -Werror main/tools/deck_notifications_test.cpp -o /tmp/deck_notifications_test
/tmp/deck_notifications_test
```

Both app and firmware require an update. Flash only the application partition
using the configured hardware profile's `app-flash`; retain SPIFFS and profiles.
On hardware, verify RAM playback, slot replacement, low-memory fallback,
USB/BLE transfer, reconnect/reboot recache, and responsive keys during uploads.
