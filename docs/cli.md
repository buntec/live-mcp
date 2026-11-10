# Command line

Besides `serve` and `install`, `live-mcp` talks to Live directly. With
`~/.local/bin` on your `PATH`:

```sh
live-mcp status
live-mcp get "live_set tracks" --fields name,mute
live-mcp describe "live_set tracks 0" --values
live-mcp set "live_set" tempo 124
live-mcp call "live_set tracks 0 clip_slots 0" create_clip 4
live-mcp get "#<id> clip_slots 0 clip" --props name,length
live-mcp watch "live_set" tempo is_playing   # print changes until Ctrl-C
```

Targets are paths from `live_set` or `live_app`, or `#ID` for an object returned
earlier. `set` values and `call` arguments are JSON, so strings need quotes:
`'"Bass"'`.

`live-mcp status` also reports `script_current: false` while Live still runs an
older Remote Script, and `ear_plugin` says whether the installed plugin matches
the one in the binary. Re-selecting the control surface doesn't load new code:
quit and restart Live.

Run `live-mcp --help` for every command and option.
