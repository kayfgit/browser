current todos.

* in the todo means its a high priority
~ in the todo means its half-done/in-progress
x in the todo means its done
? in the todo means i cant recreate the issue
/ in the todo means its not too important
- in the todo means i couldnt fix it yet and will revisit later
empty brackets means its untouched

done todos get erased shortly after.

issues:
[/] - opening spotify_player.exe and playing a song freezes :resources
[?/] - random not responding after exiting cs2 (might have to do with constant changes to resolution)
[?] - sometimes crashes after taking a windows screenshot (prntscreen button)
[/-] - opening youtube with ublock origin activated makes it open in a half-open half-not state.
       CONFIRMED cause: uBOLite's `ublock-filters` main-world SCRIPTLET (its YouTube fetch-response
       rewrite / #reloadxhr player-fetch trick), NOT its network/DNR blocking. Only fires on the
       logged-in profile + certain videos; page hangs at readyState=loading so the player boots but
       metadata/comments never hydrate. Reload works (served from cache). Fix when revisiting: make
       uBOL skip YouTube (rename the youtube.com entries in rulesets/scripting/scriptlet/main/
       ublock-filters.js $scriptletHostnames$ -- index-preserving) and let native ADBLOCK_JS json-prune
       handle YT ads. NOTE: a separate, worse variant (netblock stalling ALL YT videos in default mode)
       was already fixed -- netblock is now gated to :adblock native only.
[] - recent history with H/L isnt working
[x] - make caret mode be a block cursor instead of a caret (ironic i know)
[x] - currently entering caret mode starts selecting instantly, it should only enable caret mode, if i want to select something they press "v" again.
[x] - theres a bug where the caret grows really long the more i select
[x] - if the cursor on caret mode reaches the bottom of the screen, it doesnt bring the screen down with it, so i cant see anything below (it needs to act like vim's cursor where if the cursor reaches the bottom it scrolls down to see more content)

feats:
[*] - :save/saved or :favorite/favorites for saving a page for later
[*] - allow :te customization

maybes:
[~] - allow :ai to completely customize the browser, for example "the commandbar is too small, make it 25% taller and change the background color to green" or "change X keybind to Y"
[] - maybe add a :engine command to change browser engine? might be overkill and dont know if its possible
[] - ctrl+: enters command bar in vim mode, allows vim motions.
