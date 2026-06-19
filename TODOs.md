current todos.

* in the todo means its a high priority
~ in the todo means its half-done/in-progress
x in the todo means its done
nothing means its untouched

done todos gets erased shortly after.

issues:
[*] - fix other languages missing characters (like japanese characters) on :read and :term
[*] - opening spotify_player.exe and playing a song freezes :resources
[] - random not responding after exiting cs2 (might have to do with constant changes to resolution)
[] - i pressed o and i together and the browser froze in a state of half normal and half insert mode, had to alt+f4
[] - sometimes crashes after taking a windows screenshot (prntscreen button)
[*] - fix fullscreen, the commandbar gets cut off on the bottom. 
[*] - H/L to go back/forth in history doesnt work on :read 
[*] - current history only goes back once, needs to have the full history (for example if i go into google, search youtube and click on youtube.com and then hit H to go back multiple times, it only goes back to the youtube search and not google)
[] - when splitting it creates a new tab, it should work like tmux in which the tabs are contained in one tab
[*] - cant press youtube buttons when on normal mode, when i double click it quickly opens and closes
[*] - sometimes when changing modes through normal to passthrough to term and vice versa it freezes and i cant leave the passthrough mode, only fix is to alt+tab to regain control

feat:
[~] - look for a way to make the browser consume the least amount of resources from the pc, even with the heavy engine running
[] - :video command that toggles videos and accepts urls typing ":video" disables video (just like in :research mode), typing ":video <url-that-contains-a-video>" would open mpv or something similar to play the video
[] - :commandbar to toggle visibility
[] - command bar colors for each mode
[] - currently the tab and command bars get hidden when entering fullscreen, add a command toggle for it
[~] - :history command to see all past history and manipulate it (go back or delete everything) -- DONE: :history opens a vim buffer of the full visited list; :history clear / :clear history wipes it. TODO: press Enter on a line to open that entry.
[*] - show the url on the right side of the command bar when hovering a link
[x] - shorten the current url in command bar, only show full if the mouse hovers it or the command bar is opened
[x] - allow mouse support on the command bar, highlighting copying etc.
[*] - ctrl+: enters command bar in vim mode, allows vim motions.

maybes:
[] - upgrade :ai so that it can maybe allow for browser manipulation, like "layout for web development" based on prior chat of "for web dev i like my browser on the left and split terminals on the right"
[*] - allow :ai to completely customize the browser, for example "the commandbar is too small, make it 25% taller and change the background color to green" or "change X keybind to Y"
[] - maybe add a :engine command to change browser engine? might be overkill and dont know if its possible
[] - maybe add commands that changes the page in-real-time, like ":images" would toggle images, ":videos" would toggle videos, :text would toggle visible text, etc
