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
[*] - ship every engine with the browser by default: the installer has all engines checked and users can uncheck the ones they dont want. package each engine separately so updates only redownload the browser (~20mb) unless an engine changed, and allow adding/removing engines later (something like :engine install gecko / :engine remove servo)

issues:
[?/] - random not responding after exiting cs2 (might have to do with constant changes to resolution)
[?] - sometimes crashes after taking a windows screenshot (prntscreen button)
[?] - saving a session with :w sometimes doesnt work, no idea why
[/?] - random not responding after new update 26/07/2026 22:54
[*] - add a way for users to freely install extensions
[] - after running ":ai open github and wikipedia side by side please" it opened github.com on the left, splitted vertically but didnt open wikipedia, and then gave me: "! groq 429: Rate limit reached for model `openai/gpt-oss-120b` in organization `org_01kva1e35pet7tkmy9tte02502` service tier `on_demand` on tokens per minute (TPM): Limit 8000, Used 5616, Requested 5181. Please try again in 20.9775s. Need more tokens? Upgrade to Dev Tier today at https://console.groq.com/settings/billing"
[] - random not responding 08/10/2026 i forgot the time
[x] - pressing the right mouse button on a youtube video opens both the shells menu and the youtube's menu, should only open youtube's own menu
[x] - normal mode on terminal has that annoying "[COPY] hjkl/w/b move..." text, need that removed.

feats:
[] - make ";" toggle browser hud visibility
[/] - add vertical sidebar like zen browser
[*] - make it so every browser related error is shown in :error/:errors (like if an engine crashes/errors, extension stops working, everything)
[x] - allow splitting without having anything open (like on the welcome screen), basically treat the welcome screen as an empty pane that cannot be closed if its by itself, but closeable if theres other splits, if the user closes every split then they go back to the welcome screen
[x] - allow clicking on a external link and opening it in the browser

maybes:
[~] - allow :ai to completely customize the browser, for example "the commandbar is too small, make it 25% taller and change the background color to green" or "change X keybind to Y"
[~*] - maybe add a :engine command to change browser engine? might be overkill and dont know if its possible
[/] - ctrl+: enters command bar in vim mode, allows vim motions on the command bar itself.
[] - maybe change the :read command into a :html toggle like :js and :css? if you think about it, removing html is basically removing the dom parsing and rendering only text, which is exactly what :read does
[] - maybe add the zen browser glance feature, where i can preview a link in a floating window
[] - maybe add webkit as a third engine, so web devs on windows/linux can check their sites against apple's engine without a mac. use playwright's prebuilt windows webkit build (downloaded only on :engine install webkit) instead of building it ourselves. its webkit, not safari: no apple fonts, some codecs missing, no ios touch/viewport stuff, so it catches most "broken in safari" bugs but not all. first do a quick spike to check it can draw inside our panes
[] - maybe add gecko (firefox) as an engine too. mozilla has no desktop embedding api (geckoview is android only), so it'd run as a separate firefox process (playwright ships a patched windows firefox) driven remotely with its window put inside our pane. that's the same plumbing webkit needs, so build that external-engine host once and use it for both
[] - maybe add trident (ie11's engine, mshtml) as an engine. it's built into every windows install (no download), embeds with the old webbrowser control into a child window like our panes, and microsoft supports it until at least 2029. useful for people stuck maintaining old enterprise/government/bank intranet sites. testing only, not for browsing (old security). cheapest engine to add, maybe i can do this one myself
[] - maybe add ladybird once it runs on windows. its a fully independent engine written from scratch off the specs, good at finding pages that lean on quirks instead of standards. its 2026 alpha is linux/macos only, windows is just a teaser for now
