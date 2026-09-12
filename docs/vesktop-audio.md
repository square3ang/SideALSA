# Vesktop screen sharing with SideALSA

Use Vesktop's built-in capture settings with the original application package.

## Entire System through the default output

In the screen-share picker, open **Open Audio Settings** and set:

| Setting | Value |
| --- | --- |
| Only Speakers | Off |
| Only Default Speakers | On |
| Ignore Inputs | On |

Then select **Entire System** as the audio source for the share.

This selects playback routed to the desktop's default sink. With
`sidealsa-line4` selected as the default output, it includes applications playing
through SideALSA Line 4. Applications explicitly routed to another Line sink are
outside this selection. In Vesktop 1.6.7, selecting those applications explicitly
in **Audio Sources** is another way to capture their PipeWire playback streams.

Make the changes in Vesktop's UI so its in-memory settings and saved settings
agree. Settings used to start a share are applied when that share is started;
editing `settings.json` behind an already running app is not a live update.

## Why these settings are needed

Vesktop 1.6.7's venmic 6.1.0 implements **Only Speakers** by requiring the
destination of an audio link to have `device.id`. SideALSA's standalone ALSA
adapters are valid `Audio/Sink` nodes with monitor ports, but have no parent
PipeWire Device object. The Only Speakers filter therefore rejects applications
playing through them.

**Only Default Speakers** instead compares the destination node with the sink
selected in PipeWire's `default.audio.sink` metadata. This works with SideALSA's
logical sinks without a parent device ID. Keeping this filter on also avoids
turning Entire System into an unrestricted traversal of audio links, which could
include microphone-to-recorder routes when both speaker filters are off.

The original classifier is visible in
[venmic 6.1.0's `on_link`](https://github.com/Vencord/venmic/blob/5481a9277da95656c79ae69c4ea1146ce10cc2d7/src/patchbay.impl.cpp).
The source and graph metadata were inspected; audio capture was not tested.

## Direct PRO / ASIO

This setup covers desktop audio flowing through PipeWire and SideALSA SHARED.
Direct ALSA PRO and ASIO audio require a separate monitoring path to appear in
PipeWire screen-share capture.

## Local package restoration

The experimental Vesktop addon patch was removed. The installed
`/usr/lib/vesktop/resources/app.asar` was restored byte-for-byte from
`app.asar.sidealsa-backup-67ea293d`; its SHA256 is
`67ea293de8673b2df7048d65e5eaddb684d794dd0e121983a6b4d724f2918cb0`.
If Vesktop loaded the experimental addon before restoration, fully quit and
reopen it to use the original module. No service restart is required for these
capture settings.
