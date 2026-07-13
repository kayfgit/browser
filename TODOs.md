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
[*] - if im on a terminal tab with long contents and i press ctrl+s to scroll up and then press escape to go into term mode, the camera isnt brought down to the latest line, instead the camera gets stuck where it was, i have to press ctrl+s and keep pressing "j" to bring the camera down and then press esc to return to normal
[*] - tabs should have a specific width, becoming smaller with the more tabs there is
[*] - if there are more than 10 tabs, find a way to switch to these tabs (maybe shift+1, to move to the eleventh tab and so on up to 20, if there are more than 20 tabs then tough luck buddy)
[x] - on the terminal, sometimes the normal mode cursor gets stuck in a place and doesnt return until i reset the mode (press ctrl+s and esc)
[x] - terminal text gets very broken if i change the zoom, it should readjust itself after zooming
[*] - toggle for the webview2 scrollbar with ":scrollbar"
[] - improve :wq, takes a while to close and i can see the default page for 1 second (it should close instantly)

feats:
[*] - :save/saved or :favorite/favorites for saving a page for later
[*] - ctrl+arrowkeys should move thee cursor through entire words, currently moves only one character at a time
[] - improve :wq, currently it only brings back webview2 tabs accurately, terminal tabs just get restarted (if possible, should maintain the folder it was before with the content inside exactly as it was before)

maybes:
[~] - allow :ai to completely customize the browser, for example "the commandbar is too small, make it 25% taller and change the background color to green" or "change X keybind to Y"
[*] - maybe add a :engine command to change browser engine? might be overkill and dont know if its possible
[/] - ctrl+: enters command bar in vim mode, allows vim motions.
