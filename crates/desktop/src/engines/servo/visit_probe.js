// Diagnostic probe for the Servo smoke test's Visit scenario: what a real site shows.
(() => {
  const body = document.body;
  const text = body ? body.innerText : '';
  const items = Array.from(document.querySelectorAll(
    'ytd-video-renderer, ytd-rich-item-renderer, ytd-rich-grid-media'));
  const sample = items.slice(0, 3).map(el => {
    const r = el.getBoundingClientRect();
    const st = getComputedStyle(el);
    const img = el.querySelector('img');
    return {
      tag: el.tagName.toLowerCase(),
      rect: [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)],
      display: st.display, visibility: st.visibility, opacity: st.opacity,
      img: img ? { src: (img.currentSrc || img.src || '').slice(0, 80), complete: img.complete,
                   natural: [img.naturalWidth, img.naturalHeight] } : null,
    };
  });
  const sized = items.filter(el => {
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  }).length;
  // Style elements the shell's scripts add (feature toggles, hints, ...).
  const shellStyles = Array.from(document.querySelectorAll('style[id], link[id]'))
    .map(el => el.id).filter(id => id.startsWith('__') || id.startsWith('browser'));
  return JSON.stringify({
    title: document.title,
    url: location.href,
    elements: document.querySelectorAll('*').length,
    images: document.images.length,
    items: items.length,
    sized,
    sample,
    shellStyles,
    features: window.__featureDefaults || null,
    errorText: /Unable to load page|something went wrong/i.test(text),
    text: text.slice(0, 400),
  });
})()
