(() => {
  const configScript = document.getElementById("overlay-config");
  const config = JSON.parse(configScript?.textContent || "{}");
  const eventsUrl = config.eventsUrl || "/api/current-print/events";
  const selectedDeviceId = (new URLSearchParams(window.location.search).get("device") || "").trim();
  const state = {
    title: document.getElementById("title"),
    fileName: document.getElementById("fileName"),
    timeEstimate: document.getElementById("timeEstimate"),
    printWeight: document.getElementById("printWeight"),
    progressPercent: document.getElementById("progressPercent"),
    layerInfo: document.getElementById("layerInfo"),
    remainingTime: document.getElementById("remainingTime"),
    progress: document.getElementById("progress"),
    toolheadTemp: document.getElementById("toolheadTemp"),
    bedTemp: document.getElementById("bedTemp"),
    fanSpeed: document.getElementById("fanSpeed"),
    printMode: document.getElementById("printMode"),
    spoolList: document.getElementById("spoolList"),
    thumbSlot: document.getElementById("thumbSlot"),
    thumbUrl: null,
    thumbPendingUrl: null,
    thumbRequest: 0,
    events: null,
    spoolIconId: 0,
  };

  function pickDevice(devices) {
    if (selectedDeviceId) {
      return devices.find((device) => device.id === selectedDeviceId) || devices[0] || null;
    }
    return devices[0] || null;
  }

  function setText(node, value) {
    node.textContent = value == null || value === "" ? "" : String(value);
  }

  function setOptionalText(node, value) {
    const text = value == null || value === "" ? "" : String(value);
    node.textContent = text;
    node.hidden = text === "";
  }

  function fallback(value, empty = "--") {
    return value == null || value === "" ? empty : String(value);
  }

  function layerText(device) {
    if (device.layerCurrent == null && device.layerTotal == null) {
      return "Layer -- / --";
    }
    return `Layer ${fallback(device.layerCurrent)} / ${fallback(device.layerTotal)}`;
  }

  function progressText(progress) {
    return progress == null ? "--%" : `${Math.round(progress)}%`;
  }

  function spoolSvg() {
    const spoolIconId = ++state.spoolIconId;
    const filamentBodyId = `spool-filament-body-${spoolIconId}`;
    const filamentClipId = `spool-filament-clip-${spoolIconId}`;
    const cutoutId = `spool-cutout-${spoolIconId}`;

    return `
      <svg viewBox="0 0 178 200" aria-hidden="true">
        <defs>
          <path id="${filamentBodyId}" d="M58 24 A66 12 0 0 1 124 24 A44.91 76 0 0 1 124 176 A66 12 0 0 1 58 176 A44.91 76 0 0 0 58 24 Z"/>
          <clipPath id="${filamentClipId}">
            <use href="#${filamentBodyId}"/>
          </clipPath>
          <path id="${cutoutId}" d="M-18 -35 A26 36 0 0 1 -29 -70 A31 18 0 0 1 0 -84 A31 18 0 0 1 29 -70 A26 36 0 0 1 18 -35 A40 40 0 0 0 -18 -35 Z"/>
        </defs>
        <ellipse cx="126" cy="100" rx="52" ry="88" fill="#4f5a70" stroke="#000" stroke-width="1" vector-effect="non-scaling-stroke"/>
        <use href="#${filamentBodyId}" fill="currentColor"/>
        <g clip-path="url(#${filamentClipId})" fill="none" stroke="#111827" stroke-width="1.6" stroke-linecap="round" opacity=".24">
          <path d="M74.5 24 A44.91 76 0 0 1 74.5 176"/>
          <path d="M91 24 A44.91 76 0 0 1 91 176"/>
          <path d="M107.5 24 A44.91 76 0 0 1 107.5 176"/>
        </g>
        <g transform="translate(58 100) scale(.6 1)">
          <circle r="88" fill="#4f5a70" stroke="#000" stroke-width="1" vector-effect="non-scaling-stroke"/>
          <g fill="currentColor" stroke="#252b3d" stroke-width="2" stroke-linejoin="round" vector-effect="non-scaling-stroke">
            <use href="#${cutoutId}" transform="translate(0 6)"/>
            <use href="#${cutoutId}" transform="rotate(120) translate(0 6)"/>
            <use href="#${cutoutId}" transform="rotate(240) translate(0 6)"/>
          </g>
          <circle r="26" fill="#111827"/>
        </g>
      </svg>
    `;
  }

  function spoolElement(spool) {
    const el = document.createElement("div");
    el.className = "spool";

    const roll = document.createElement("div");
    roll.className = "spool-roll";
    roll.style.setProperty("--spool-color", spool.color || "#9ca3af");
    roll.innerHTML = spoolSvg();

    const tag = document.createElement("span");
    tag.className = "spool-tag";
    tag.textContent = spool.label || "?";

    const material = document.createElement("span");
    material.className = "spool-material";
    material.textContent = spool.material || "Filament";

    roll.append(tag);
    el.append(roll, material);
    return el;
  }

  function renderSpools(node, spools, emptyText) {
    const items = Array.isArray(spools) ? spools.filter(Boolean) : [];
    if (items.length === 0) {
      const empty = document.createElement("div");
      empty.className = "empty";
      empty.textContent = emptyText;
      node.replaceChildren(empty);
      return;
    }

    node.replaceChildren(...items.map(spoolElement));
  }

  function renderThumb(url) {
    if (url === state.thumbUrl || url === state.thumbPendingUrl) {
      return;
    }

    const requestId = ++state.thumbRequest;
    state.thumbPendingUrl = url || null;

    if (!url) {
      state.thumbUrl = null;
      state.thumbPendingUrl = null;
      state.thumbSlot.replaceChildren();
      state.thumbSlot.className = "thumb";
      state.thumbSlot.classList.add("is-empty");
      state.thumbSlot.textContent = "3D";
      return;
    }

    const nextImage = new Image();
    nextImage.alt = "";
    nextImage.decoding = "async";
    nextImage.referrerPolicy = "no-referrer";
    nextImage.onload = () => {
      if (requestId !== state.thumbRequest || state.thumbPendingUrl !== url) {
        return;
      }

      const oldImages = Array.from(state.thumbSlot.querySelectorAll("img"));
      state.thumbSlot.className = "thumb";
      if (oldImages.length === 0) {
        state.thumbSlot.replaceChildren(nextImage);
      } else {
        state.thumbSlot.append(nextImage);
      }

      requestAnimationFrame(() => nextImage.classList.add("is-visible"));
      window.setTimeout(() => oldImages.forEach((image) => image.remove()), 220);
      state.thumbUrl = url;
      state.thumbPendingUrl = null;
    };
    nextImage.onerror = () => {
      if (requestId !== state.thumbRequest) {
        return;
      }
      state.thumbPendingUrl = null;
      if (!state.thumbUrl) {
        renderThumb(null);
      }
    };
    nextImage.src = url;
  }

  function renderError(message) {
    setText(state.title, message || "Could not load print status");
    setText(state.fileName, "--");
    setOptionalText(state.timeEstimate, "");
    setOptionalText(state.printWeight, "");
    setText(state.progressPercent, "--%");
    setOptionalText(state.layerInfo, "Layer -- / --");
    setOptionalText(state.remainingTime, "--");
    state.progress.style.width = "0%";
    setText(state.toolheadTemp, "--");
    setText(state.bedTemp, "--");
    setText(state.fanSpeed, "--");
    setText(state.printMode, "--");
    renderSpools(state.spoolList, [], "No spool data");
    renderThumb(null);
  }

  function render(data) {
    if (!data.ok) {
      renderError(data.error);
      return;
    }

    const device = pickDevice(data.devices || []);
    if (!device) {
      renderError("No printers returned");
      return;
    }

    const progress = Number.isFinite(device.progress) ? Math.max(0, Math.min(100, device.progress)) : null;
    const title = device.isPrinting ? fallback(device.title, "Unknown print") : fallback(device.title, "No active print");

    setText(state.title, title);
    setText(state.fileName, fallback(device.filename));
    setOptionalText(state.timeEstimate, device.totalPrintTime);
    setOptionalText(state.printWeight, device.weight);
    setText(state.progressPercent, progressText(progress));
    setOptionalText(state.layerInfo, layerText(device));
    setOptionalText(state.remainingTime, device.timeRemaining || "--");
    setText(state.toolheadTemp, fallback(device.toolheadTemp));
    setText(state.bedTemp, fallback(device.bedTemp));
    setText(state.fanSpeed, fallback(device.fanSpeed));
    setText(state.printMode, fallback(device.mode));
    renderSpools(
      state.spoolList,
      [...(device.amsSpools || []), ...(device.externalSpool ? [device.externalSpool] : [])],
      "No spool data",
    );

    state.progress.style.width = progress == null ? "0%" : `${progress}%`;
    renderThumb(device.thumbnail);
  }

  function handlePrintEvent(event) {
    try {
      render(JSON.parse(event.data));
    } catch (error) {
      renderError(error.message);
    }
  }

  function connectEvents() {
    if (!window.EventSource) {
      renderError("Server-sent events are not supported");
      return;
    }

    state.events = new EventSource(eventsUrl);
    state.events.addEventListener("current-print", handlePrintEvent);
    state.events.onerror = () => {
      if (state.events?.readyState === EventSource.CLOSED) {
        renderError("Print status stream closed");
      }
    };
  }

  connectEvents();
})();
