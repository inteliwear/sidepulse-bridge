# SidePulse for agents

Dot: a small USB-C device with two RGB LEDs, connected to the user's iPhone.
Signal completion, request input, or animate for fun :) The iPhone app forwards
updates to Dot; iOS may delay delivery.

SidePulse Pro: an 8-LED status indicator for a MacBook Pro's SD card slot.

`<channel-id>` = everything after `#` in the user's original link, including
`apns_` and any suffix. Keep it private. POST JSON as below.

LED syntax: `#RRGGBB duration effect`, one step per line (`\n` in JSON).
One color sets both LEDs; two set them separately. `pulse` breathes, `cosine`
fades, `none` holds; `off` clears, `repeat` loops. Limits: 512 bytes, 20 lines.

Independent timing: `index:color duration effect delay`, indexes `0`/`1`.
Join assignments with `;` to run together, each with its own duration/delay.
The next line waits for both.

**Needs input: looping amber pulse.**

    curl --fail-with-body 'https://bridge.sidepulse.io/api/leds/<channel-id>' \
      -H 'Content-Type: application/json' \
      -d '{"leds":"off\n#FF3A00 1.6s pulse\nrepeat\n"}'

Alternative `.LED` programs: put the text in the same request's `leds` field.

**Working: looping cyan pulses, 760 ms each; LED 0 starts immediately, LED 1 after 260 ms.**

    off 320ms cosine
    0:#00E5FF 760ms pulse 0ms; 1:#00E5FF 760ms pulse 260ms
    repeat

**Fun: looping magenta (LED 0, 300 ms) and cyan (LED 1, 1200 ms, delayed 600 ms).**

    off
    0:#FF00FF 300ms pulse 0ms; 1:#00E5FF 1200ms pulse 600ms
    repeat

**Done: two green pulses, then off; plays once.**

    off
    #00FF66 #00FF66 240ms pulse
    #000000 #000000 140ms none
    #00FF66 #00FF66 640ms pulse
    #000000 #000000 400ms none

[Full animation spec, including individual timing/delays](https://github.com/inteliwear/sidepulse/blob/main/LEDS_FORMAT.md#delays-and-staggering).
