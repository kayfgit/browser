// Servo only: page-side cosmetic hiding and YouTube ad handling, for an engine with no
// extension support. WebView2 tabs get all of this from uBlock Origin Lite instead. Servo
// blocks ad requests natively (engines/servo/netblock.rs); the redirect/popup guard is
// NAVGUARD_JS.
(function () {
  if (window.__adblockInit) return;
  window.__adblockInit = true;
  var on = (typeof window.__adblockDefault === 'undefined') ? true : !!window.__adblockDefault;

  // --- NitroPay neutraliser (the fixed left/right "skin" rails + anchor bar) ---
  // Many wikis (e.g. taskbarhero.wiki) serve ads via NitroPay: the page plants an
  // inline `window.nitroAds` stub that QUEUES createAd() calls until
  // s.nitropay.com/ads-*.js loads and renders them — including the fixed side rails
  // and the bottom anchor (the dismissible-with-an-X popups). Those containers have
  // site-specific ids, so cosmetic selectors are unreliable; instead we kill the
  // source. This init script runs at document-start, BEFORE that inline stub, so we
  // plant our OWN inert nitroAds first and lock it with a non-configurable accessor:
  // the page's `nitroAds = nitroAds || {…}` and the loader can't replace it, createAd
  // is a no-op that never resolves, and the queue swallows pushes — so no ad is ever
  // created even if the loader script itself slips past the host blocklist. The host
  // is also in AD_HOSTS so the network/DOM-sweep layers attack the script too.
  if (on) {
    try {
      var nzStub = {
        createAd: function () { return new Promise(function () {}); },
        addUserToken: function () {},
        abortAd: function () {},
        queue: { push: function () {}, length: 0 },
        loaded: true,
      };
      Object.defineProperty(window, 'nitroAds', {
        configurable: false,
        get: function () { return nzStub; },
        set: function () {},
      });
    } catch (e) {}
  }

  // --- DOM cosmetic observer: hide ad containers as the page builds itself ------
  // Ad scripts are blocked at the network level (engines/servo/netblock.rs); this hides
  // what's left.
  // The page-side job here is to HIDE ad containers cosmetically, and — on
  // YouTube — to remove the anti-adblock enforcement modal the INSTANT it's inserted,
  // before it can paint (the observer fires before the next render, so no 0.3s flash).
  function observe() {
    if (!document.documentElement) { setTimeout(observe, 0); return; }
    new MutationObserver(function (muts) {
      if (!on) return;
      for (var i = 0; i < muts.length; i++) {
        var a = muts[i].addedNodes;
        for (var j = 0; j < a.length; j++) {
          var n = a[j];
          if (n.nodeType !== 1) continue;
          hideCosmetic(n);
          if (isYT) killEnforcement(n);
        }
      }
    }).observe(document.documentElement, { childList: true, subtree: true });
  }
  observe();

  // --- YouTube VIDEO ads: prune the ad descriptors from the player response ----
  // uBlock's technique (`json-prune`): YouTube schedules pre-/mid-roll ads from
  // `adPlacements`/`playerAds`/`adSlots` keys in the player API JSON. We trap the
  // ways that JSON reaches the page and delete those keys, so the player simply
  // has no ad to play — no seeking, so no "1s ad then black screen". Only patched
  // on YouTube to avoid overhead/risk elsewhere.
  var isYT = location.hostname.indexOf('youtube.com') !== -1 ||
             location.hostname.indexOf('youtube-nocookie.com') !== -1;
  // The CANONICAL uBO key set — and only these. Earlier we also pruned
  // `adParams`/`adBreakHeartbeatParams`/`importantHeaders`; that over-reach mangled
  // the response shape (importantHeaders isn't even an ad key) and is exactly the kind
  // of tampering YouTube's anti-adblock detector trips on, which is what was raising
  // the "Ad blockers violate ToS" wall. Prune the ads, leave the rest intact.
  var YT_AD_KEYS = ['adPlacements', 'playerAds', 'adSlots'];
  // The enforcement wall ITSELF arrives as data: YouTube delivers it in the player
  // response's `playabilityStatus` (status != OK + an errorScreen carrying the
  // "ad blocker" message). Neutralise it at the source so the popup never renders and
  // playback proceeds — but ONLY for the ad-block case. We gate on the response actually
  // mentioning ad-blocking so genuine unavailability (private/age/region/deleted) still
  // errors correctly instead of being forced to a broken "OK" with no streams.
  function fixPlayability(data) {
    var ps = data.playabilityStatus;
    if (!on || !ps || typeof ps !== 'object' || !ps.status || ps.status === 'OK') return;
    // Only lift the wall when there are real streams to REVEAL. If YouTube withheld
    // `streamingData` (the detected case), forcing "OK" just yields a black screen —
    // worse than its actual message — so leave it. With ad telemetry now flowing
    // (see AD_HOSTS) detection shouldn't fire at all; this is the belt-and-braces path
    // for when the wall rides along with an otherwise-playable response.
    if (!data.streamingData) return;
    try {
      var blob = JSON.stringify(ps).toLowerCase();
      if (blob.indexOf('ad block') !== -1 || blob.indexOf('ad-block') !== -1 ||
          blob.indexOf('adblock') !== -1) {
        ps.status = 'OK';
        delete ps.errorScreen;
        delete ps.reason;
        delete ps.messages;
      }
    } catch (e) {}
  }
  function prunePlayerAds(data) {
    if (!on || !data || typeof data !== 'object') return data;
    try {
      for (var i = 0; i < YT_AD_KEYS.length; i++) {
        if (data[YT_AD_KEYS[i]] != null) delete data[YT_AD_KEYS[i]];
      }
      if (data.playabilityStatus) fixPlayability(data);
      if (data.playerResponse) prunePlayerAds(data.playerResponse);
      if (Array.isArray(data)) for (var j = 0; j < data.length; j++) prunePlayerAds(data[j]);
    } catch (e) {}
    return data;
  }
  // DO NOT PATCH NATIVES HERE. This block used to wrap `JSON.parse` and
  // `Response.prototype.json` so every player response got pruned on the way past.
  // Bisected 2026-08-03: those two wrappers are what broke YouTube. With either one
  // installed the player never gets media (`readyState` stays 0, `duration` NaN), and
  // after ~6s YouTube gives up and auto-advances the playlist — which is the
  // black-screen / no-audio / "page keeps reloading and lands on a different video"
  // report. It is NOT the pruning: with `YT_AD_KEYS` emptied so the walk deletes
  // nothing, merely having the wrappers in place still killed playback. Replacing a
  // native with a function whose `toString()` is no longer `[native code]` is a
  // well-known thing YouTube's integrity checks look for, and it clearly acts on it.
  //
  // Verified against the same watch URL, everything else enabled: wrappers on → 2-3
  // playlist skips per 22s, `ready=0`; wrappers off → 0 skips, video plays.
  //
  // The accessor below is a DIFFERENT mechanism — it defines a property that doesn't
  // exist yet rather than replacing a built-in — and was measured innocent (removing it
  // while the wrappers stayed did not help; keeping it with the wrappers gone plays
  // fine). It covers the case that matters most anyway: a full page load embeds the
  // player response as this inline global, which is where pre-roll ads are scheduled.
  // Ads on subsequent SPA navigations are handled by the `youtube()` tick instead.
  if (isYT) {
    try {
      var _ytIPR;
      Object.defineProperty(window, 'ytInitialPlayerResponse', {
        configurable: true,
        get: function () { return _ytIPR; },
        set: function (v) { _ytIPR = prunePlayerAds(v); }
      });
    } catch (e) {}
  }

  // --- cosmetic: hide ad containers + YouTube ad slots -------------------------
  // We hide IMPERATIVELY (inline `display:none`) rather than via an injected
  // `<style>`: a strict CSP (YouTube's) blocks injected stylesheets, but inline
  // styles set through `el.style` are exempt — so this works where a stylesheet
  // silently wouldn't. Hidden nodes are tagged so `:ads` off can restore them.
  var COSMETIC = [
    'ins.adsbygoogle', '.adsbygoogle',
    'iframe[src*="doubleclick"]', 'iframe[src*="googlesyndication"]',
    'iframe[id^="google_ads_"]', 'iframe[id*="aswift"]',
    'div[id^="google_ads_"]', 'div[id^="div-gpt-ad"]',
    '[data-ad-slot]', '[data-ad-client]', '[aria-label="Advertisement"]',
    '.ad-container', '.ad-banner', '.ad-wrapper', '.ads-container',
    '.advertisement', '.sponsored-content', '.trc_related_container',
    // YouTube: the sponsored/promoted feed cards, the masthead banner, in-player
    // ad slots. `:has()` removes the empty grid cell the ad lived in, not just the
    // ad node, so the feed doesn't keep a blank gap.
    'ytd-display-ad-renderer', 'ytd-promoted-sparmus-renderer',
    'ytd-promoted-video-renderer', 'ytd-ad-slot-renderer',
    'ytd-in-feed-ad-layout-renderer', 'ytd-banner-promo-renderer',
    'ytd-statement-banner-renderer', 'ad-slot-renderer',
    '#masthead-ad', '#player-ads',
    '.ytp-ad-overlay-container', '.ytp-ad-module', '.video-ads',
    '.ytp-ad-overlay-slot', '.ytp-suggested-action'
  ];
  // `:has()` selectors are kept SEPARATE: an engine that rejected one would throw
  // for the whole `querySelectorAll`, so a bad one here can't disable the core list
  // above. These remove the empty feed cell the ad lived in (not just the ad node).
  var COSMETIC_HAS = [
    'ytd-rich-item-renderer:has(ytd-ad-slot-renderer)',
    'ytd-rich-item-renderer:has(ytd-in-feed-ad-layout-renderer)',
    'ytd-rich-section-renderer:has(ytd-statement-banner-renderer)'
  ];
  var COSMETIC_SEL = COSMETIC.join(',');
  var COSMETIC_HAS_SEL = COSMETIC_HAS.join(',');
  function hideEl(el) {
    if (el.getAttribute('data-adblock-hidden')) return;
    el.setAttribute('data-adblock-hidden', '1');
    el.style.setProperty('display', 'none', 'important');
  }
  function hideBySelector(root, selector) {
    try {
      if (root.nodeType === 1 && root.matches && root.matches(selector)) hideEl(root);
      if (root.querySelectorAll) {
        var hits = root.querySelectorAll(selector);
        for (var i = 0; i < hits.length; i++) hideEl(hits[i]);
      }
    } catch (e) {}
  }
  function hideCosmetic(root) {
    if (!on || !root) return;
    hideBySelector(root, COSMETIC_SEL);
    hideBySelector(root, COSMETIC_HAS_SEL);
  }
  function unhideCosmetic() {
    try {
      var hits = document.querySelectorAll('[data-adblock-hidden]');
      for (var i = 0; i < hits.length; i++) {
        hits[i].removeAttribute('data-adblock-hidden');
        hits[i].style.removeProperty('display');
      }
    } catch (e) {}
  }

  // --- YouTube: skip the skippable, seek past the unskippable ------------------
  // The dangerous half of this is the seek. YouTube plays ads through the SAME
  // `<video>` element as the feature, so the `ad-showing` class is the ONLY thing
  // separating "seek the ad to its end" from "end the user's video". If that class is
  // stale or set a beat early/late, seeking to `duration` fires `ended` on the real
  // video — and on a playlist (or with autoplay) YouTube then advances to the NEXT
  // video, which reads as a page that reloads itself forever and lands on something
  // else entirely. So the seek is now cross-checked, and never fires on anything that
  // looks like the feature.
  //
  // `lengthSeconds` from the player response is the feature's length; an ad loaded in
  // the same element has a different duration. If what's loaded matches the feature,
  // the class is lying — refuse. As an independent backstop (the player response can
  // be stale across an SPA navigation) anything longer than AD_MAX_SECS is treated as
  // the feature too: real pre-/mid-rolls are seconds-to-a-couple-minutes long, so the
  // worst case of the bound is one unusually long ad we let play rather than one video
  // we destroy.
  var AD_MAX_SECS = 360;
  function featureLength() {
    try {
      var d = window.ytInitialPlayerResponse && window.ytInitialPlayerResponse.videoDetails;
      var n = d && parseFloat(d.lengthSeconds);
      return (isFinite(n) && n > 0) ? n : 0;
    } catch (e) { return 0; }
  }
  // Mute is forced only for the duration of an ad, and the page's own setting is put
  // back afterwards — the old code muted and never unmuted, so a single misfire left
  // the video silent for the rest of the session.
  var premuted = null;
  function restoreMute(v) {
    if (premuted === null || !v) return;
    try { v.muted = premuted; } catch (e) {}
    premuted = null;
  }
  function youtube() {
    if (!on || location.hostname.indexOf('youtube.com') === -1) return;
    try {
      var skip = document.querySelector(
        '.ytp-ad-skip-button, .ytp-ad-skip-button-modern, .ytp-skip-ad-button');
      if (skip) skip.click();
      var player = document.querySelector('.html5-video-player');
      var v = document.querySelector('video.html5-main-video');
      var adPlaying = !!player && player.classList.contains('ad-showing');
      if (adPlaying && v && v.duration && isFinite(v.duration)) {
        var feat = featureLength();
        var isFeature = (feat && Math.abs(v.duration - feat) < 2) || v.duration > AD_MAX_SECS;
        if (!isFeature) {
          if (premuted === null) premuted = v.muted;
          v.muted = true;
          v.currentTime = v.duration;
        }
      } else {
        restoreMute(v);
      }
      var close = document.querySelector('.ytp-ad-overlay-close-button');
      if (close) close.click();
      killEnforcement(document);
    } catch (e) {}
  }

  // The dismissible "ad blockers are not allowed" MODAL that rides over a playing video.
  // We match it by its dedicated COMPONENT TAG, not by text — that's how uBO does it, and
  // it's why this works in every language (the old English-text gate let the PT/ES/etc.
  // versions through). `ytd-enforcement-message-view-model` is used for nothing else, so
  // removing its enclosing popup + scrim is safe. Returns whether one was found+removed.
  // Called from the MutationObserver the instant the modal is INSERTED (so it never paints
  // — killing the ~0.3s flash) and from the YouTube tick as a backstop. `root` is the node
  // to scan (an inserted subtree, or `document`).
  function killEnforcement(root) {
    if (!on || !isYT) return false;
    try {
      var sel = 'ytd-enforcement-message-view-model, ytd-enforcement-message-view-model-renderer';
      var enf = [];
      if (root.matches && root.matches(sel)) enf.push(root);
      if (root.querySelectorAll) {
        var hits = root.querySelectorAll(sel);
        for (var i = 0; i < hits.length; i++) enf.push(hits[i]);
      }
      if (!enf.length) return false;
      for (var d = 0; d < enf.length; d++) {
        (enf[d].closest('ytd-popup-container') ||
         enf[d].closest('tp-yt-paper-dialog') || enf[d]).remove();
      }
      // Drop the scrim too, else the page stays click-blocked / scroll-locked, and
      // resume playback if YouTube paused it behind the wall.
      var bd = document.querySelector('tp-yt-iron-overlay-backdrop');
      if (bd) bd.remove();
      var vid = document.querySelector('video.html5-main-video');
      if (vid && vid.paused) { try { vid.play(); } catch (e) {} }
      return true;
    } catch (e) {}
    return false;
  }

  // Periodic backstop. YouTube needs a FAST poll — youtube() skips in-player ads, seeks
  // past unskippables and strips the enforcement modal, all hinging on player state the
  // MutationObserver (childList only) can't see. Everywhere else the observer already
  // hides inserted ad containers as they appear, so a slow pass (catching ad classes
  // toggled onto existing nodes) is plenty — 4x fewer idle wakeups per non-YouTube tab.
  function tick() { if (!on) return; hideCosmetic(document); youtube(); }
  hideCosmetic(document);
  document.addEventListener('DOMContentLoaded', function () { hideCosmetic(document); });
  setInterval(tick, isYT ? 500 : 2000);

  // --- live toggle from the shell (`:ads`), no reload needed -------------------
  // Called by `NAVGUARD_JS`'s `__setAdblock`, which owns the toggle.
  window.__adblockCosmetic = function (v) {
    on = !!v;
    if (on) { hideCosmetic(document); youtube(); }
    else { unhideCosmetic(); }
  };
})();
