current todos.

* in the todo means its a high priority
~ in the todo means its half-done/in-progress
x in the todo means its done
? in the todo means i cant recreate the issue
nothing means its untouched

done todos gets erased shortly after.

issues:
[~] - youtube anti-adblock wall / black screen: ROOT CAUSE was blocking YT's own first-party ad telemetry (api/stats/ads, ptracking, get_midroll) — that's what YT's detector checks, so it served a stream-less enforcement response. Fix (uBO's approach): let the telemetry flow, kill ads purely by pruning the player-response JSON (adPlacements/playerAds/adSlots). playabilityStatus un-wall only fires when streams exist (no more black screen). NEEDS LIVE VERIFY — cat-and-mouse
[] - opening spotify_player.exe and playing a song freezes :resources
[?] - random not responding after exiting cs2 (might have to do with constant changes to resolution)
[?] - i pressed o and i together and the browser froze in a state of half normal and half insert mode, had to alt+f4
[?] - sometimes crashes after taking a windows screenshot (prntscreen button)
[] - when splitting it creates a new tab, it should work like tmux in which the tabs are contained in one tab
[x] - hint mode conflicting in terminal mode, there should be NO hint mode when pressin f/F in terminal, only vim's "find"
[x] - remove the "[page] keys -> page * esc: shell" status everytime something happens in a webpage

feat:
[] - ctrl+: enters command bar in vim mode, allows vim motions.
[x] - :freeze command that freezes the browser in place to consume the least ammount of ram possible even with a bunch of webview2 tabs opened, :unfreeze returns to normal (useful when a lot of things are consuming ram but you dont want to close the browser)
[] - :save/saved or :favorite/favorites for saving a page for later

maybes:
[~] - allow :ai to completely customize the browser, for example "the commandbar is too small, make it 25% taller and change the background color to green" or "change X keybind to Y"
[] - maybe add a :engine command to change browser engine? might be overkill and dont know if its possible
