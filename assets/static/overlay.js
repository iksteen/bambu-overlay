(() => {
  const configScript = document.getElementById("overlay-config");
  const config = JSON.parse(configScript?.textContent || "{}");
  const eventsUrl = config.eventsUrl || "/api/current-print/events";
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
  };

  function pickDevice(devices) {
    return devices.find((device) => device.isPrinting) || devices[0] || null;
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
    return `
      <svg viewBox="0 0 48 48" aria-hidden="true">
        <circle cx="24" cy="24" r="18" fill="currentColor" opacity="0.92"/>
        <circle cx="24" cy="24" r="7" fill="rgba(9, 13, 16, 0.82)" stroke="rgba(255,255,255,0.5)" stroke-width="2"/>
        <path d="M24 6a18 18 0 0 1 16 26" fill="none" stroke="rgba(255,255,255,0.62)" stroke-width="3" stroke-linecap="round"/>
        <path d="M16 39a18 18 0 0 1-8-17" fill="none" stroke="rgba(0,0,0,0.28)" stroke-width="3" stroke-linecap="round"/>
        <path d="M12 24c0-7 5-12 12-12s12 5 12 12-5 12-12 12" fill="none" stroke="rgba(255,255,255,0.22)" stroke-width="2" stroke-linecap="round"/>
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
