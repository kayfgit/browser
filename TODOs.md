current todos.

* in the todo means its a high priority, the more there is, the highest the chance it will be implemented on the next update
~ in the todo means its half-done/in-progress
x in the todo means its done (or if it just disappears)
? in the todo means i cant recreate the issue
/ in the todo means its not too important
- in the todo means i couldnt fix it yet and will revisit later
$ in the todo means its an easy fix/feat
empty brackets means its untouched (also means i probably just dumped a random idea/issue i had)

done todos get erased shortly after.

STUFF TO KEEP IN MIND:
- because i made this for myself first, its only ported to windows which is my daily driver. if someone wants ill port it to lsomeone wants ill port it to linux/mac aswell
- i dont really care about looks (will probably be one of the last things i care about)
- i dont really care about security *as of now* (of course i care about it, but i have to make the browser actually usable before making it secure)
- i REALLY care about efficiency
- i REALLY care about resource usage
- i REALLY care about speed
- i REALLY care about how lightweight it is
- I REALLY CARE ABOUT MALLEABILITY, users should be able to do absolutely anything they want with it

general:
[*] - ship only webview2 by default: the installer has all engines unchecked and users can check the ones they want, or install/remove later with :engine install <engine> and :engine remove <engine>

issues:
[?/] - random not responding after exiting cs2 (might have to do with constant changes to resolution)
[?] - sometimes crashes after taking a windows screenshot (prntscreen button)
[/?] - random not responding after new update 26/07/2026 22:54
[*] - add a way for users to freely install extensions
[?] - random not responding 08/10/2026 i forgot the time
[] - inline history shows up on :open, but not :open -t

feats:
[*] - make every browser related warning/error/message show in :warn/:err/:msg (like if an engine crashes/errors, extension stops working, everything)
[] - allow dragging links into the browser that opens in a new tab on the default engine
[] - some buttons for back/forward history and reload on the top bar (also vertical bar), to make it usable with the mouse aswell (even though the focus is keyboard only). should be completely customizable

maybes:
[~] - allow :ai to completely customize the browser, for example "the commandbar is too small, make it 25% taller and change the background color to green" or "change X keybind to Y"
[~*] - maybe add a :engine command to change browser engine? might be overkill and dont know if its possible
[/] - ctrl+: enters command bar in vim mode, allows vim motions on the command bar itself.
[] - maybe change the :read command into :html with a toggle like :js and :css? if you think about it, removing html is basically removing the dom parsing and rendering only text, which is exactly what :read does
[] - maybe add the zen browser glance feature, where i can preview a link in a floating window
[] - maybe add webkit as a third engine, so web devs on windows/linux can check their sites against apple's engine without a mac. use playwright's prebuilt windows webkit build (downloaded only on :engine install webkit) instead of building it ourselves. its webkit, not safari: no apple fonts, some codecs missing, no ios touch/viewport stuff, so it catches most "broken in safari" bugs but not all. first do a quick spike to check it can draw inside our panes
[] - maybe add gecko (firefox) as an engine too. mozilla has no desktop embedding api (geckoview is android only), so it'd run as a separate firefox process (playwright ships a patched windows firefox) driven remotely with its window put inside our pane. that's the same plumbing webkit needs, so build that external-engine host once and use it for both
[] - maybe add ladybird once it runs on windows. its a fully independent engine written from scratch off the specs, good at finding pages that lean on quirks instead of standards. its 2026 alpha is linux/macos only, windows is just a teaser for now
