'use strict';

/*
 * 影院前端：海报墙 / 悬停预览 / 播放页 + 标签检索（M2）。
 *
 * 业务规则一律来自后端命令（阈值来自 L2，流程来自 L1），本文件只做呈现与转发：
 * - 悬停 300ms 触发预览，30 秒或移开即停（阈值取自 `ui_constants`）；
 * - 预览实例由后端隔离（常驻静音、不写进度），前端不参与其状态；
 * - 影片画面由后端原生子窗口承载，因此「移开即停」不依赖 DOM 的 mouseleave
 *   （画面窗口会挡住指针），而是依据页面重新收到 mousemove 时的坐标判断；
 * - 落在画面上的点击由原生窗口截获后经事件转发回来（preview-click / player-click）；
 * - 检索（FR-03）的组合语义（同维度 OR、跨维度 AND）由 L2 唯一实现，
 *   本文件只提交「文本 + 维度筛选 + 排序」，维度与取值清单也全部来自后端配置，
 *   因此新增一个标签维度不需要改动本文件。
 */

const TAURI = window.__TAURI__;
const invoke = TAURI ? TAURI.core.invoke : null;
const listen = TAURI ? TAURI.event.listen : null;

/** 物理像素与 CSS 像素的换算系数：后端窗口/画面坐标全部以物理像素计。 */
const DPR = window.devicePixelRatio || 1;

const els = {};
[
  'dir-input', 'dir-browse', 'add-btn', 'scan-btn', 'data-btn', 'online-btn', 'preview-toggle', 'stats',
  'data-modal', 'data-path', 'data-export', 'data-import', 'data-cancel',
  'scan-overlay', 'scan-title', 'scan-bar-fill', 'scan-detail', 'scan-current', 'scan-cancel',
  'online-modal', 'online-url', 'online-import', 'online-cancel', 'online-result',
  'searchbar', 'search-input', 'filter-toggle', 'multi-toggle', 'sort-select', 'size-group', 'filter-bar', 'dims',
  'wall', 'empty', 'grid', 'sources',
  'select-bar', 'select-count', 'select-enter', 'select-cancel',
  'multi-bar', 'layout-bar', 'multi-count', 'limit-select', 'concurrent-audio', 'browse-btn', 'exit-multi', 'multi-stage',
  'multi-exit-modal', 'exit-keep', 'exit-stop', 'exit-cancel',
  'resume-modal', 'resume-title', 'resume-desc', 'resume-continue', 'resume-restart', 'resume-cancel',
  'player-bar', 'pb-title', 'pb-toggle', 'pb-seek', 'pb-time', 'pb-close',
  'pb-subtitle', 'pb-sublive', 'pb-add-multi',
  'subtitle-overlay', 'sub-origin', 'sub-translation',
  'subtitle-menu', 'sm-close', 'sm-state', 'sm-tracks', 'sm-notice', 'sm-queue',
  'sm-origin-lang', 'sm-target-lang', 'sm-display-group', 'sm-start', 'sm-save', 'sm-stop',
  'subtitle-privacy', 'sub-privacy-note', 'sub-engine-list', 'sub-privacy-confirm', 'sub-privacy-cancel',
  'subtitle-failure', 'sub-failure-desc', 'sub-fail-external', 'sub-fail-dictation', 'sub-fail-close',
  'subtitle-external', 'sub-external-path', 'sub-external-confirm', 'sub-external-cancel',
  'preview-note', 'toast',
  // 主机模式（Console）
  'console', 'console-btn', 'c-top-stats', 'c-clock', 'c-search-btn', 'c-search', 'c-settings', 'c-to-classic',
  'c-screen-home', 'c-screen-lib', 'c-hero', 'c-cover-row', 'c-chips', 'c-wide-row',
  'c-chips-lib', 'c-grid', 'c-empty',
  'c-settings-panel', 'c-settings-close', 'c-preview-toggle', 'c-size-group', 'c-to-classic-2',
  // 顶栏（主机模式需整层隐藏）
  'topbar',
].forEach((id) => { els[id] = document.getElementById(id); });

/** 阈值来自后端 L2 常量，前端不自行定义。 */
let constants = { previewTriggerMs: 300, previewSegmentMs: 30000, controlBarPx: 76 };

let items = [];
let playingId = null;
let paused = false;
let tickTimer = null;
let seeking = false;
let currentDurationMs = 0;
/** 上一次已提示过的在线源故障，避免同一条故障每 400ms 弹一次。 */
let lastFault = null;

// 检索状态：维度与取值清单来自后端，选中项与关键字只在本地保存（不持久化）
let dims = [];            // [{ id, label, selector, source, order, editable, lockable, tags: [{id,name,count}] }]
let selected = {};        // dimensionId -> [取值名称]
let searchText = '';
let sortTag = 'recent_added';
let expandedDims = new Set(); // 已展开「更多标签」的维度
let searchTimer = null;

/** 检索区每个维度默认展示的取值上限，超出折叠为「更多标签」。 */
const TAG_PREVIEW = 12;

/** 海报尺寸 → 网格最小列宽（CSS 像素）。 */
const CARD_MIN_PX = { large: 216, mid: 168, small: 124 };

// 悬停预览状态
let hoverTimer = null;
let previewItemId = null;
let previewStopTimer = null;
let previewRect = null; // 物理像素 { x, y, w, h }
let previewToken = 0;

// 续播弹层
let resumeItem = null;
let resumeOffer = null;

let toastTimer = null;

// ---------------------------------------------------------------------------
// 多屏状态（FR-06）
// ---------------------------------------------------------------------------
// 业务规则不在这里重定义：格位数、上限、发声归属、拒绝原因全部来自后端命令。

let selectMode = false;        // 海报墙多选态
let selectedIds = [];          // 选中条目 id（有序，决定进入多屏时的屏位）
let inMulti = false;           // 多屏视图是否激活
let browsingLibrary = false;   // 多屏播放中浏览海报墙（原生画面让位）
let multiState = null;         // 最近一次 multi_status / multi_* 返回的整体视图
let multiLayouts = [];         // 可用布局（来自 multi_layouts）
let dimsOpen = false;          // 维度标签面板的用户开关
let multiBusy = false;         // 进入多屏 / 切换布局时的重入保护
let pendingSlot = null;        // 「更换影片 / 空位添加」待填充的屏位
const pausedSlots = new Set(); // 本端记录的各格暂停态（后端视图不单列该字段）
const draggingSlots = new Set(); // 正在拖动进度的屏位（拖动期间不被轮询覆盖）
const faultHidden = new Set(); // 已因故障隐藏原生画面窗口的屏位
const audioCache = {};         // 屏位 → { itemId, tracks }（某格音轨清单缓存）
const rateOf = {};             // 屏位 → 倍速（本端记录，回显用）
let previewNoteTimer = null;

const toPhysical = (value) => Math.round(value * DPR);

function formatTime(ms) {
  const total = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const mm = String(m).padStart(2, '0');
  const ss = String(s).padStart(2, '0');
  return h > 0 ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
}

function showToast(message, ms = 2600) {
  els.toast.textContent = message;
  els.toast.classList.remove('hidden');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => els.toast.classList.add('hidden'), ms);
}

/** 占位海报的色相：由条目 id 稳定派生，同一影片每次颜色一致。 */
function hueOf(id) {
  let hash = 0;
  for (let i = 0; i < id.length; i += 1) {
    hash = (hash * 31 + id.charCodeAt(i)) >>> 0;
  }
  return hash % 360;
}

// ---------------------------------------------------------------------------
// 海报墙
// ---------------------------------------------------------------------------

async function refresh() {
  // 先取库统计：空状态要据此区分「库为空」与「筛选无结果」
  await refreshStats();
  let result = [];
  try {
    result = await invoke('search_items', { query: currentQuery() });
  } catch (err) {
    showToast(String(err));
  }
  items = Array.isArray(result) ? result : [];
  // 主机模式可见时改由主机层渲染（同一份查询状态，呈现方式不同）；
  // 主机层还需要一份「全库 + 最近观看排序」的数据，故顺带刷新它。
  if (consoleShown()) {
    await loadConsoleData();
    renderConsole();
  } else {
    renderGrid();
  }
  await refreshSources();
}

/** 提交给后端的查询：组合语义（同维度 OR、跨维度 AND）与排序全部由 L1/L2 决定。 */
function currentQuery() {
  const filters = {};
  for (const [dimensionId, values] of Object.entries(selected)) {
    if (values.length) {
      filters[dimensionId] = values.slice();
    }
  }
  return { text: searchText, filters, sort: sortTag };
}

function renderGrid() {
  els.grid.replaceChildren();
  for (const item of items) {
    els.grid.appendChild(buildCard(item));
  }
  renderEmpty();
}

// ---------------------------------------------------------------------------
// 检索（PRD FR-03：标签筛选 + 搜索 + 空状态）
// ---------------------------------------------------------------------------

/** 生效筛选的扁平清单，供已选标签条与空状态列出。 */
function activeEntries() {
  const out = [];
  for (const dim of dims) {
    for (const value of selected[dim.id] || []) {
      out.push({ dimensionId: dim.id, label: dim.label, value });
    }
  }
  return out;
}

function hasQuery() {
  return searchText.trim() !== '' || activeEntries().length > 0;
}

/**
 * 拉取维度与取值清单（取值按使用频次降序，见后端 `list_dimension_tags`）。
 * 维度是配置、取值是数据：新增一个维度只需要后端配置多一行，本文件一行不改。
 */
async function reloadFilters() {
  try {
    const list = await invoke('list_dimensions');
    dims = await Promise.all(
      list.map(async (dim) => {
        let tags = [];
        try {
          tags = await invoke('list_dimension_tags', { dimensionId: dim.id });
        } catch (err) {
          tags = [];
        }
        return { ...dim, tags };
      }),
    );
  } catch (err) {
    dims = [];
  }
  renderDims();
  renderFilterBar();
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

/** 维度面板里的取值标签：点击即筛选，再点一次取消（PRD FR-03「单标签点击」）。 */
function tagChip(dim, name, count) {
  const on = (selected[dim.id] || []).includes(name);
  const chip = el('button', 'chip' + (on ? ' on' : ''));
  chip.type = 'button';
  chip.appendChild(document.createTextNode(name));
  if (typeof count === 'number' && count > 0) {
    chip.appendChild(el('span', 'cnt', String(count)));
  }
  chip.title = `${dim.label} · ${name}`;
  chip.addEventListener('click', () => toggleTag(dim.id, name));
  return chip;
}

/** 已选标签条 / 空状态里的移除入口。 */
function removeChip(entry) {
  const chip = el('button', 'chip on');
  chip.type = 'button';
  chip.appendChild(document.createTextNode(`${entry.label} · ${entry.value}`));
  chip.appendChild(el('span', 'chip-x', '×'));
  chip.title = `移除「${entry.label} · ${entry.value}」`;
  chip.addEventListener('click', () => toggleTag(entry.dimensionId, entry.value));
  return chip;
}

function toggleTag(dimensionId, value) {
  const values = selected[dimensionId] || (selected[dimensionId] = []);
  const at = values.indexOf(value);
  if (at >= 0) {
    values.splice(at, 1);
  } else {
    values.push(value);
  }
  if (!values.length) {
    delete selected[dimensionId];
  }
  renderFilterBar();
  renderDims();
  refresh();
}

function clearFilters() {
  selected = {};
  renderFilterBar();
  renderDims();
  refresh();
}

function renderFilterBar() {
  const entries = activeEntries();
  els['filter-bar'].replaceChildren();
  els['filter-bar'].classList.toggle('hidden', entries.length === 0);
  if (!entries.length) {
    return;
  }
  els['filter-bar'].appendChild(el('span', 'fb-lead', '已选标签'));
  for (const entry of entries) {
    els['filter-bar'].appendChild(removeChip(entry));
  }
  const clear = el('button', 'fb-clear', '清空');
  clear.type = 'button';
  clear.addEventListener('click', clearFilters);
  els['filter-bar'].appendChild(clear);
}

function renderDims() {
  els.dims.replaceChildren();
  const activeCount = activeEntries().length;
  els['filter-toggle'].textContent = activeCount ? `标签筛选（${activeCount}）` : '标签筛选';

  let shown = 0;
  for (const dim of dims) {
    const tags = dim.tags || [];
    if (!tags.length) {
      continue; // 尚无取值的维度不占位
    }
    shown += 1;

    const section = el('div', 'dim');
    const head = el('div', 'dim-head');
    head.appendChild(el('span', 'dim-label', dim.label));
    if (dim.source === 'derived') {
      // 派生维度由系统按规则归集，用户不能手动编辑（PRD FR-03 标签体系表）
      head.appendChild(el('span', 'dim-note', '系统自动'));
    }
    section.appendChild(head);

    const body = el('div', 'dim-tags');
    const expanded = expandedDims.has(dim.id);
    const visible = expanded ? tags : tags.slice(0, TAG_PREVIEW);
    for (const tag of visible) {
      body.appendChild(tagChip(dim, tag.name, tag.count));
    }
    // 已选但当前清单里已不存在的取值照样显示：不自动放宽条件
    for (const value of selected[dim.id] || []) {
      if (!tags.some((tag) => tag.name === value)) {
        body.appendChild(tagChip(dim, value));
      }
    }

    if (tags.length > TAG_PREVIEW) {
      const more = el('button', 'chip more');
      more.type = 'button';
      more.textContent = expanded ? '收起' : `更多标签（${tags.length - TAG_PREVIEW}）`;
      more.addEventListener('click', () => {
        if (expanded) {
          expandedDims.delete(dim.id);
        } else {
          expandedDims.add(dim.id);
        }
        renderDims();
      });
      body.appendChild(more);
    }

    section.appendChild(body);
    els.dims.appendChild(section);
  }

  if (!shown) {
    els.dims.appendChild(
      el('div', 'empty-sub', '暂无标签：先扫描入库，派生维度会随入库自动生成取值。'),
    );
  }
}

function applyPosterSize(size) {
  const min = CARD_MIN_PX[size] || CARD_MIN_PX.mid;
  els.grid.style.setProperty('--card-min', `${min}px`);
  for (const btn of els['size-group'].querySelectorAll('.seg-btn')) {
    btn.classList.toggle('on', btn.dataset.size === size);
  }
  // 主机模式：封面卡 / 宽幅卡 / 网格三处尺寸同一档位联动（设置浮层与经典界面共用）
  const c = CONSOLE_CARD_PX[size] || CONSOLE_CARD_PX.mid;
  els['console'].style.setProperty('--c-cover-w', `${c.cover}px`);
  els['console'].style.setProperty('--c-wide-w', `${c.wide}px`);
  els['console'].style.setProperty('--c-grid-min', `${c.grid}px`);
  for (const btn of els['c-size-group'].querySelectorAll('.seg-btn')) {
    btn.classList.toggle('on', btn.dataset.size === size);
  }
}

/** 空状态：库为空时给入库引导；筛选/搜索无结果时列出生效标签与移除入口（FR-03）。 */
function renderEmpty() {
  if (items.length) {
    els.empty.classList.add('hidden');
    return;
  }
  els.empty.classList.remove('hidden');
  els.empty.replaceChildren();

  if (!hasQuery()) {
    els.empty.appendChild(el('div', 'empty-title', '还没有内容'));
    els.empty.appendChild(
      el('div', 'empty-sub', '在上方点「浏览…」选择本地目录，点「添加目录」，再点「扫描入库」。'),
    );
    return;
  }

  els.empty.appendChild(el('div', 'empty-title', '没有符合条件的影片'));
  els.empty.appendChild(
    el('div', 'empty-sub', '同维度内多选为「或」，跨维度之间为「与」；此处不会自动放宽条件。'),
  );
  if (searchText.trim()) {
    els.empty.appendChild(el('div', 'empty-sub', `搜索词：${searchText.trim()}`));
  }

  const entries = activeEntries();
  if (entries.length) {
    const chips = el('div', 'empty-chips');
    for (const entry of entries) {
      chips.appendChild(removeChip(entry));
    }
    els.empty.appendChild(chips);
  }

  const actions = el('div', 'empty-actions');
  const clear = el('button', 'btn primary', '清空筛选');
  clear.type = 'button';
  clear.addEventListener('click', clearFilters);
  actions.appendChild(clear);
  if (searchText.trim()) {
    const clearText = el('button', 'btn ghost', '清除搜索');
    clearText.type = 'button';
    clearText.addEventListener('click', () => {
      els['search-input'].value = '';
      searchText = '';
      refresh();
    });
    actions.appendChild(clearText);
  }
  els.empty.appendChild(actions);
}

/** 封面 data URL 缓存：同一条目只向后端取一次封面字节（后端也按「文件是否存在」跳过抓取）。 */
const coverCache = new Map();

/**
 * 取真实封面并贴到海报位上；取不到（后端返回空串 / 命令失败）就移除图片、
 * 保留后端背景生成的占位图——封面属「尽力而为」，不影响卡片其它信息。
 *
 * 后端 `item_cover` 现在**只读本地文件、不联网**（封面由导入后的一次后台任务从
 * 列表页卡片逐张下载），所以每张卡片可以各自独立重试、不必串行排队：请求本身很廉价，
 * 第一次没命中会顺手挂起一次「按源抓封面」，之后各卡片陆续能拿到。
 */
function loadCover(itemId, img) {
  (async () => {
    const cached = coverCache.get(itemId);
    if (cached) {
      img.src = cached;
      return;
    }
    // 后台按源抓封面是**串行**的（一次列表页 ≈ 每张 1~3s），整页几十张可能耗时两三分钟，
    // 因此重试窗口要给足：最多 60 次 × 3s ≈ 180s，够后台把整页封面下完；命中即停。
    // 注意：`buildCard` 先调用本函数、卡片随后才被 append 进 DOM，因此这里**不能**在
    // 首次取值之前检查 `img.isConnected`——那会让每张卡片都直接早退、永不调用后端。
    // 检查放到每次取值之后：只在真正要贴图 / 继续重试前判断卡片是否还在页面上。
    for (let attempt = 0; attempt < 60; attempt += 1) {
      let src = '';
      try {
        src = await invoke('item_cover', { itemId });
      } catch (err) {
        src = '';
      }
      if (src) {
        if (img.isConnected) {
          coverCache.set(itemId, src);
          img.src = src;
        }
        return;
      }
      if (!img.isConnected) {
        return; // 卡片已被重渲染 / 切走：放弃取值，等新卡片再取
      }
      await sleep(3000);
    }
    // 反复取不到：移除图片，保留占位图（封面属「尽力而为」）
    img.remove();
  })();
}

/** 等待若干毫秒（封面重试用）。 */
function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function buildCard(item) {
  const card = document.createElement('div');
  card.className = 'card' + (item.missing ? ' missing' : '');
  card.dataset.id = item.id;
  card.title = item.title;

  const poster = document.createElement('div');
  poster.className = 'poster';
  const hue = hueOf(item.id);
  poster.style.background =
    `linear-gradient(158deg, hsl(${hue}, 40%, 27%), hsl(${(hue + 42) % 360}, 34%, 14%))`;
  const initial = document.createElement('div');
  initial.className = 'initial';
  initial.textContent = (item.title || '影').trim().charAt(0).toUpperCase();
  poster.appendChild(initial);
  // 有真实封面（在线站点源抓到并缓存到本机）时叠加图片；取不到就保留上面的占位图
  if (item.hasPoster) {
    const img = document.createElement('img');
    img.className = 'poster-img';
    img.alt = item.title || '封面';
    img.loading = 'lazy';
    img.decoding = 'async';
    poster.appendChild(img);
    loadCover(item.id, img);
  }
  card.appendChild(poster);

  if (item.durationMs) {
    const dur = document.createElement('div');
    dur.className = 'dur';
    dur.textContent = formatTime(item.durationMs);
    card.appendChild(dur);
  }

  if (item.missing) {
    card.appendChild(flag('missing', '文件缺失'));
  } else if (item.watched) {
    card.appendChild(flag('watched', '已看'));
  }

  if (item.durationMs && item.positionMs > 0) {
    const bar = document.createElement('div');
    bar.className = 'progress';
    const fill = document.createElement('i');
    fill.style.width = `${Math.min(100, (item.positionMs / item.durationMs) * 100).toFixed(1)}%`;
    bar.appendChild(fill);
    card.appendChild(bar);
  }

  const title = document.createElement('div');
  title.className = 'title';
  title.textContent = item.title;
  card.appendChild(title);

  // 多选态的选择标记：显示为选中序号（同时决定进入多屏时的屏位）
  const pick = document.createElement('div');
  pick.className = 'pick';
  card.appendChild(pick);
  markSelected(card, item);

  card.addEventListener('mouseenter', () => {
    if (selectMode) return; // 多选态只为挑选，不触发悬停预览
    scheduleHover(item, card);
  });
  card.addEventListener('mouseleave', cancelHover);
  card.addEventListener('click', () => onCardClick(item));

  return card;
}

/** 单张海报的选中态与序号（多选态下可见）。 */
function markSelected(card, item) {
  const at = selectedIds.indexOf(item.id);
  card.classList.toggle('selected', at >= 0);
  const pick = card.querySelector('.pick');
  if (pick) pick.textContent = at >= 0 ? String(at + 1) : '';
}

/** 海报点击分三种语义：多选态挑片、多屏浏览库时加入空位、否则进入单路播放。 */
function onCardClick(item) {
  if (selectMode) {
    toggleSelect(item);
    return;
  }
  if (inMulti) {
    addCardToMulti(item);
    return;
  }
  openItem(item);
}

function flag(kind, text) {
  const node = document.createElement('div');
  node.className = `flag ${kind}`;
  node.textContent = text;
  return node;
}

// ---------------------------------------------------------------------------
// 悬停预览（PRD FR-02）
// ---------------------------------------------------------------------------

function scheduleHover(item, card) {
  cancelHover();
  if (consoleOn) {
    return; // 主机模式不做悬停预览：鼠标语义在大屏导航下失效，且预览与多屏共享解码额度
  }
  hoverTimer = setTimeout(() => startPreview(item, card), constants.previewTriggerMs);
}

function cancelHover() {
  clearTimeout(hoverTimer);
  hoverTimer = null;
}

async function startPreview(item, card) {
  if (playingId || item.missing || !els['preview-toggle'].checked) {
    return;
  }
  // 多屏播放期间一律不发起预览请求（文案与 L1 的拒绝原因一致）：
  // 预览与多屏共享同一份解码额度，且不得抢占正在播放的窗口。
  if (multiState && multiState.multiScreen) {
    showPreviewNote(card, '多屏播放期间不提供悬停预览');
    return;
  }
  const rect = card.getBoundingClientRect();
  const x = toPhysical(rect.left);
  const y = toPhysical(rect.top);
  const width = toPhysical(rect.width);
  const height = toPhysical(rect.height);

  const token = (previewToken += 1);
  let result;
  try {
    result = await invoke('start_preview', { itemId: item.id, x, y, width, height });
  } catch (err) {
    showToast(String(err));
    return;
  }
  // 请求期间指针已移开或已切到别的海报：立即收掉这一路
  if (token !== previewToken) {
    invoke('stop_preview').catch(() => {});
    return;
  }
  if (!result.started) {
    previewItemId = null;
    previewRect = null;
    // 拒绝不是静默失败：在海报浮层标注原因（PRD FR-02 / §11）
    if (result.reason && !String(result.reason).includes('关闭')) {
      showPreviewNote(card, result.reason);
    }
    return;
  }

  previewItemId = item.id;
  previewRect = { x, y, w: width, h: height };
  clearTimeout(previewStopTimer);
  previewStopTimer = setTimeout(stopPreview, constants.previewSegmentMs);
}

/** 在对应海报上方标注「预览不可用」的原因；不做重试、不抢占正在播放的窗口。 */
function showPreviewNote(card, reason) {
  const rect = card.getBoundingClientRect();
  els['preview-note'].textContent = reason;
  els['preview-note'].style.left = `${rect.left + rect.width / 2}px`;
  els['preview-note'].style.top = `${rect.top - 6}px`;
  els['preview-note'].classList.remove('hidden');
  clearTimeout(previewNoteTimer);
  previewNoteTimer = setTimeout(hidePreviewNote, 4200);
}

function hidePreviewNote() {
  clearTimeout(previewNoteTimer);
  previewNoteTimer = null;
  els['preview-note'].classList.add('hidden');
}

function stopPreview() {
  cancelHover();
  clearTimeout(previewStopTimer);
  previewStopTimer = null;
  previewToken += 1;
  const wasActive = previewItemId !== null;
  previewItemId = null;
  previewRect = null;
  if (wasActive) {
    invoke('stop_preview').catch(() => {});
  }
}

/** 指针已不在预览矩形内（画面窗口挡住了指针，重新收到事件即意味着离开了）。 */
function pointerOutsidePreview(event) {
  if (!previewRect) {
    return false;
  }
  const px = toPhysical(event.clientX);
  const py = toPhysical(event.clientY);
  const tol = 2;
  return (
    px < previewRect.x - tol ||
    py < previewRect.y - tol ||
    px > previewRect.x + previewRect.w + tol ||
    py > previewRect.y + previewRect.h + tol
  );
}

// ---------------------------------------------------------------------------
// 播放（PRD FR-04）
// ---------------------------------------------------------------------------

async function openItem(item) {
  if (item.missing) {
    showToast('文件缺失，无法播放');
    return;
  }
  stopPreview();

  let offer = null;
  try {
    offer = await invoke('resume_offer', { itemId: item.id });
  } catch (err) {
    showToast(String(err));
    return;
  }

  if (offer) {
    resumeItem = item;
    resumeOffer = offer;
    els['resume-desc'].textContent =
      `${item.title} · 已观看 ${offer.percent}%，从 ${formatTime(offer.positionMs)} 继续`;
    els['resume-modal'].classList.remove('hidden');
    return;
  }
  await play(item.id, null, item);
}

function hideResume() {
  els['resume-modal'].classList.add('hidden');
  resumeItem = null;
  resumeOffer = null;
}

async function play(itemId, startMs, item) {
  // 单路播放的字幕作用对象恒为主上下文
  subTarget = null;
  // 切换条目时先收掉上一场实时字幕（未成句由后端自动落库）
  await closeSubtitleSession();
  let status;
  try {
    status = await invoke('open_player', { itemId, startMs });
  } catch (err) {
    showToast(String(err));
    return;
  }

  playingId = itemId;
  paused = false;
  currentDurationMs = status.durationMs || 0;
  els['pb-title'].textContent = item ? item.title : (items.find((i) => i.id === itemId) || {}).title || '—';
  els['pb-toggle'].textContent = '暂停';
  els['pb-seek'].value = 0;
  updateTimeUi(status.positionMs || 0, currentDurationMs);
  els['player-bar'].classList.remove('hidden');
  applyView();
  startTick();
  // 打开成功后取入口可见性 + 已存轨道：有外挂/生成轨道时按其句时间轴复用渲染
  await initSubtitleForItem(itemId);
}

/** 统一心跳：单路走 `tick`，多屏走 `multi_status`（两者都由后端顺带落库进度）。 */
function startTick() {
  stopTick();
  tickTimer = setInterval(() => {
    if (inMulti) multiTick();
    else tick();
  }, 400);
}

function stopTick() {
  if (tickTimer) {
    clearInterval(tickTimer);
    tickTimer = null;
  }
}

async function tick() {
  if (!playingId) {
    return;
  }
  let status;
  try {
    status = await invoke('tick');
  } catch (err) {
    return;
  }
  if (!status.playing) {
    return;
  }
  // 在线源（边下边播）：取流失败必须让用户看见原因，而不是「进度条在、画面不来」
  if (status.fault && status.fault !== lastFault) {
    lastFault = status.fault;
    showToast(status.fault);
  } else if (!status.fault) {
    lastFault = null;
  }
  currentDurationMs = status.durationMs || currentDurationMs;
  if (!seeking && currentDurationMs > 0) {
    els['pb-seek'].value = Math.round((status.positionMs / currentDurationMs) * 1000);
  }
  // 影片还在下载 / 尚未装载成功：时长未知时提示「正在准备」，避免看起来像卡住
  if (status.buffering && !currentDurationMs) {
    els['pb-time'].textContent = '正在准备…';
  } else {
    updateTimeUi(status.positionMs, currentDurationMs);
  }
  // 实时字幕轮询只在播放页打开且字幕处于活动状态时才发起，避免无谓开销
  maybeSubtitleTick(status.positionMs || 0);
  if (status.ended) {
    await closePlayer();
  }
}

function updateTimeUi(positionMs, durationMs) {
  els['pb-time'].textContent = `${formatTime(positionMs)} / ${durationMs ? formatTime(durationMs) : '--:--'}`;
}

async function togglePause() {
  if (!playingId) {
    return;
  }
  paused = !paused;
  els['pb-toggle'].textContent = paused ? '播放' : '暂停';
  try {
    await invoke('set_paused', { paused });
  } catch (err) {
    showToast(String(err));
  }
}

async function closePlayer() {
  stopTick();
  stopPreview();
  // 关闭播放页前收口实时字幕：未成句时后端会自动落库为轨道，据返回值提示
  await closeSubtitleSession();
  hideSubtitleUi();
  const id = playingId;
  playingId = null;
  currentDurationMs = 0;
  lastFault = null;
  els['player-bar'].classList.add('hidden');
  applyView();
  refreshSubtitleUi();
  try {
    await invoke('close_player');
  } catch (err) {
    showToast(String(err));
  }
  if (id) {
    await refresh();
    // 观看状态维度（未看 / 在看 / 已看）的取值与频次随进度变化
    await reloadFilters();
  }
  // 若此前处于主机模式，播放结束后让主机层按当前状态回归
  syncConsole();
}

// ---------------------------------------------------------------------------
// 入库（PRD FR-01）
// ---------------------------------------------------------------------------

// 文件夹选择：由 tauri-plugin-dialog 提供原生对话框，用户直接选目录而不必复制粘贴路径。
// 插件未就绪（或非 Tauri 环境）时返回 null，调用方回退到手动输入。
async function pickFolder(title) {
  const dialog = TAURI && TAURI.dialog;
  if (!dialog || typeof dialog.open !== 'function') {
    showToast('当前环境不支持文件夹选择，请手动填写目录路径');
    return null;
  }
  try {
    const picked = await dialog.open({
      directory: true,
      multiple: false,
      title: title || '选择本地目录',
    });
    return typeof picked === 'string' ? picked : null;
  } catch (err) {
    showToast(String(err));
    return null;
  }
}

// 「浏览…」：选出目录后只回填输入框，交由用户确认后再点「添加目录」。
async function browseDir() {
  const picked = await pickFolder('选择要入库的本地目录');
  if (picked) {
    els['dir-input'].value = picked;
  }
}

async function addSource() {
  let path = els['dir-input'].value.trim();
  if (!path) {
    // 路径为空时直接弹系统文件夹选择器：用户只需点路径所在的文件夹即可
    path = await pickFolder('选择要入库的本地目录');
    if (!path) {
      return;
    }
    els['dir-input'].value = path;
  }
  els['add-btn'].disabled = true;
  try {
    await invoke('add_local_source', { path });
    els['dir-input'].value = '';
    await refreshSources();
    showToast('已添加目录，点「扫描入库」开始导入');
  } catch (err) {
    showToast(String(err));
  } finally {
    els['add-btn'].disabled = false;
  }
}

// ---------------------------------------------------------------------------
// 扫描进度浮层（FR-01）
//
// 进度完全来自后端 `scan-progress` 事件（L3 逐目录上报、L1 逐条探测上报），
// 前端不做任何估算：估算的进度条在「整块硬盘」这种目录树上只会骗人。
// ---------------------------------------------------------------------------

/** 本次扫描正在处理的源名，拼在标题前缀上（进度事件只带源 id）。 */
let scanTargetName = '';

function openScanOverlay() {
  scanTargetName = '';
  els['scan-title'].textContent = '正在扫描';
  els['scan-detail'].textContent = '正在枚举目录…';
  els['scan-current'].textContent = '';
  els['scan-bar-fill'].style.width = '0';
  els['scan-bar-fill'].classList.add('pending');
  els['scan-cancel'].disabled = false;
  els['scan-cancel'].textContent = '取消扫描';
  els['scan-overlay'].classList.remove('hidden');
}

function closeScanOverlay() {
  els['scan-overlay'].classList.add('hidden');
  els['scan-bar-fill'].classList.remove('pending');
}

/** 把一条 `scan-progress` 事件渲染到浮层上。 */
function renderScanProgress(p) {
  if (!p) {
    return;
  }
  const prefix = scanTargetName ? `${scanTargetName} · ` : '';
  const skipped = p.skipped ? ` · 跳过 ${p.skipped} 项` : '';

  if (p.stage === 'probing') {
    // 探测阶段有确定的分母：画确定进度条，用户能看到「还剩几条」
    const total = Math.max(1, p.probeTotal || 0);
    const done = Math.min(p.probeCurrent || 0, total);
    els['scan-bar-fill'].classList.remove('pending');
    els['scan-bar-fill'].style.width = `${Math.round((done / total) * 100)}%`;
    els['scan-title'].textContent = `${prefix}正在探测时长（${done} / ${total}）`;
    els['scan-detail'].textContent = `已入库 ${p.discovered} 条${skipped}`;
  } else {
    // 发现阶段总量未知：不确定进度条 + 计数，不假装有分母
    els['scan-bar-fill'].classList.add('pending');
    els['scan-title'].textContent = p.cancelled
      ? `${prefix}正在取消…`
      : `${prefix}正在扫描目录`;
    els['scan-detail'].textContent =
      `已扫描 ${p.dirsScanned} 个目录 · 已读 ${p.filesSeen} 个文件 · 已入库 ${p.discovered} 条${skipped}`;
  }

  els['scan-current'].textContent = p.current || '';
  els['scan-current'].title = p.current || '';
}

async function cancelScan() {
  els['scan-cancel'].disabled = true;
  els['scan-cancel'].textContent = '正在取消…';
  try {
    await invoke('cancel_scan');
  } catch (err) {
    showToast(String(err));
  }
}

async function scanAll() {
  els['scan-btn'].disabled = true;
  openScanOverlay();
  // 先挂监听再发命令：后端在命令入口就会推第一条进度，晚了会丢掉这次起点
  const unlisten = listen
    ? await listen('scan-progress', (event) => renderScanProgress(event.payload))
    : null;
  try {
    const sources = await invoke('list_sources');
    if (!sources.length) {
      showToast('先添加一个本地目录');
      return;
    }
    let stored = 0;
    let skipped = 0;
    let failed = 0;
    let cancelled = false;
    for (const source of sources) {
      scanTargetName = source.name;
      try {
        const report = await invoke('scan_source', { id: source.id });
        stored += report.stored;
        skipped += report.skipped || 0;
        if (report.cancelled) {
          // 取消后不再扫下一个源：用户要的是「停下来」，不是「换个源继续」
          cancelled = true;
          break;
        }
      } catch (err) {
        failed += 1;
        showToast(`${source.name} 扫描失败：${err}`);
      }
    }
    if (cancelled) {
      showToast(`已取消扫描：本次入库 ${stored} 条`);
    } else if (!failed) {
      showToast(`扫描完成：共 ${stored} 条${skipped ? `，跳过 ${skipped} 项` : ''}`);
    }
    await refresh();
    // 入库会生成派生标签（时长区间 / 画质 / 字幕状态），取值清单随之变化
    await reloadFilters();
  } catch (err) {
    showToast(String(err));
  } finally {
    if (unlisten) {
      unlisten();
    }
    closeScanOverlay();
    els['scan-btn'].disabled = false;
  }
}

// 数据迁移（PRD FR-07）：导出库到目录 / 从目录导入。冲突与缺失的判定都在后端，
// 本文件只提交路径并把回执翻译成一句提示。
function openDataModal() {
  els['data-path'].value = '';
  els['data-modal'].classList.remove('hidden');
  els['data-path'].focus();
}

function hideDataModal() {
  els['data-modal'].classList.add('hidden');
}

function portabilitySummary(report) {
  const parts = [
    `源 ${report.sources}`,
    `条目 ${report.items}`,
    `用户标签 ${report.userTags}`,
    `收藏 ${report.collections}`,
    `设置 ${report.settings}`,
    `字幕 ${report.subtitleTracks}`,
  ];
  if (report.itemsMissing) {
    parts.push(`缺失 ${report.itemsMissing}`);
  }
  return parts.join(' · ');
}

async function exportLibrary() {
  const dest = els['data-path'].value.trim();
  if (!dest) {
    showToast('请填写导出目录');
    return;
  }
  els['data-export'].disabled = true;
  try {
    const report = await invoke('export_library', { dest });
    hideDataModal();
    showToast(`已导出：${portabilitySummary(report)}`);
  } catch (err) {
    showToast(String(err));
  } finally {
    els['data-export'].disabled = false;
  }
}

async function importLibrary() {
  const src = els['data-path'].value.trim();
  if (!src) {
    showToast('请填写导出包目录');
    return;
  }
  els['data-import'].disabled = true;
  try {
    const report = await invoke('import_library', { src });
    hideDataModal();
    showToast(`已导入：${portabilitySummary(report)}`);
    // 导入会补入条目、用户标签与派生标签，检索区取值清单需要刷新
    await refresh();
    await refreshSources();
    await reloadFilters();
    await refreshStats();
  } catch (err) {
    showToast(String(err));
  } finally {
    els['data-import'].disabled = false;
  }
}

// 联网导入（M7）：粘贴列表页 URL → 后端用 yt-dlp 枚举条目并缓存封面。
// 支持多行：逐条提交，每条各自登记一个在线站点源；失败只提示该条，不影响其它条。
function openOnlineModal() {
  els['online-modal'].classList.remove('hidden');
  els['online-result'].textContent = '';
  els['online-url'].focus();
}

function hideOnlineModal() {
  els['online-modal'].classList.add('hidden');
}

async function onlineImport() {
  const urls = els['online-url'].value
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean);
  if (!urls.length) {
    showToast('请粘贴列表页地址（每行一个）');
    return;
  }
  els['online-import'].disabled = true;
  els['online-result'].textContent = '正在导入，请稍候…';
  let discovered = 0;
  let stored = 0;
  let failed = 0;
  try {
    for (const url of urls) {
      try {
        const report = await invoke('online_import', { url });
        discovered += report.discovered;
        stored += report.stored;
        els['online-result'].textContent =
          `${report.sourceName}：发现 ${report.discovered} 条，入库 ${report.stored} 条`;
      } catch (err) {
        failed += 1;
        els['online-result'].textContent = `${url} 导入失败：${err}`;
      }
    }
    // 条目、海报封面与源清单都发生了变化，整体刷新一次（refresh 内含统计与源清单）
    coverCache.clear();
    await refresh();
    await reloadFilters();
    if (!failed) {
      hideOnlineModal();
      showToast(`联网导入完成：发现 ${discovered} 条，入库 ${stored} 条`);
    }
  } finally {
    els['online-import'].disabled = false;
  }
}

async function refreshSources() {
  let sources = [];
  try {
    sources = await invoke('list_sources');
  } catch (err) {
    return;
  }
  els.sources.replaceChildren();
  if (!sources.length) {
    els.sources.textContent = '尚未添加媒体源';
    return;
  }
  for (const source of sources) {
    const node = document.createElement('span');
    const failed = source.state === 'scan_failed';
    node.className = 'src' + (failed ? ' failed' : '');
    // 扫描失败时按 PRD FR-01 要求提示原因（lastError 由后端 last_error 列给出）。
    let suffix = '';
    if (failed) {
      suffix = source.lastError ? ` · 扫描失败（${source.lastError}）` : ' · 扫描失败';
    }
    node.textContent = `${source.name}（${source.location}）${suffix}`;
    els.sources.appendChild(node);
  }
}

async function refreshStats() {
  try {
    const stats = await invoke('library_stats');
    libraryStats = stats;
    els.stats.textContent =
      `源 ${stats.sources} · 条目 ${stats.items} · 解码器 ${stats.decoderReady ? '就绪' : '不可用'}`;
    renderConsoleTopStats();
  } catch (err) {
    /* 统计失败不影响主流程 */
  }
}

// ---------------------------------------------------------------------------
// 实时字幕（PRD FR-05）
// ---------------------------------------------------------------------------
// 渲染规则以后端返回的 display 为准（L1 资源降级会把 bilingual 改成 originOnly，
// 前端必须跟随）。本地只保留「最近一句」的状态，DOM 每帧幂等更新、不重建节点。
// 入口可见性、隐私告知、失败三出口、轨道复用与降级提示全部由后端数据驱动。

const SUB_STATE_TEXT = {
  idle: '未开启',
  initializing: '初始化中',
  running: '运行中',
  paused: '已暂停',
  saved: '已保存为轨道',
  failed: '失败',
};
const SUB_KIND_TEXT = { external: '外挂', generated: '实时生成', embedded: '内嵌' };

/**
 * 字幕上下文：单路播放（以及未开启并发发声时的多屏）用 `null`（主上下文），
 * 并发发声模式下多屏每格各用屏位号。每个上下文各自持有会话状态，
 * 因此多格可以同时识别与显示（与后端「主上下文 + 每格上下文」一一对应）。
 */
function subKeyOf(target) {
  return target == null ? 'main' : `slot${target}`;
}

function newSubContext() {
  return {
    info: null,             // subtitle_info() / poll 汇总
    tracks: [],             // 当前条目字幕轨道
    entryVisible: false,    // 「实时字幕」入口是否可见（无内嵌且无外挂）
    reuseTrackId: null,     // 当前复用的轨道 id
    reuseTable: [],         // 复用轨道按时间排序的句子表
    reuseIndex: -1,
    reuseLine: null,        // { origin, translation }
    lastOrigin: '',
    lastTranslation: '',
    lastPartial: '',
    degradedNotice: '',
    qualityNotified: false, // 质量低提示同一次会话只给一次
    polling: false,         // 避免轮询重叠
    engineId: null,
    engineLocality: 'local',
  };
}

const subContexts = new Map(); // 上下文键 → 会话状态
let subTarget = null;          // 字幕菜单当前作用对象：null = 主上下文；数字 = 屏位

/** 取（必要时创建）某个作用对象的字幕上下文。 */
function subCtx(target = subTarget) {
  const key = subKeyOf(target);
  let ctx = subContexts.get(key);
  if (!ctx) {
    ctx = newSubContext();
    subContexts.set(key, ctx);
  }
  return ctx;
}

/**
 * 某格的字幕作用对象：并发发声模式下每格独立（屏位号）；
 * 否则多屏只有发声窗口一路（会话在主上下文，与后端收口一致）；
 * 非发声格返回 `undefined`（该格不可用实时字幕）。
 */
function slotSubTarget(slot) {
  if (multiState && multiState.concurrentAudio) return slot;
  return multiState && multiState.speaker === slot ? null : undefined;
}

/** 实时字幕是否处于活动状态（后端仍在推进识别或保护性暂停）。 */
function liveOn(target = subTarget) {
  const ctx = subCtx(target);
  return !!(
    ctx.info &&
    ctx.info.enabled &&
    (ctx.info.state === 'initializing' || ctx.info.state === 'running' || ctx.info.state === 'paused')
  );
}

function resetSubtitleLocal(target = subTarget) {
  subContexts.set(subKeyOf(target), newSubContext());
}

/** 退出多屏 / 切换发声模式：清空全部上下文，回到主上下文。 */
function resetAllSubtitleContexts() {
  subContexts.clear();
  subTarget = null;
}

function resetSubtitleText(target = subTarget) {
  const ctx = subCtx(target);
  ctx.lastOrigin = '';
  ctx.lastTranslation = '';
  ctx.lastPartial = '';
  ctx.reuseIndex = -1;
  ctx.reuseLine = null;
}

/** 某作用对象当前播放的条目 id：主上下文取单路播放 / 发声窗口，格上下文取该格。 */
function subItemId(target) {
  if (target == null) {
    if (playingId) return playingId;
    if (multiState && multiState.speaker != null) {
      const view = multiState.slots.find((s) => s.slot === multiState.speaker);
      if (view) return view.itemId;
    }
    return null;
  }
  const view = multiState && multiState.slots.find((s) => s.slot === target);
  return view ? view.itemId : null;
}

/** 打开播放页 / 某格字幕菜单后：拉入口可见性 + 轨道 + 状态，并选定复用轨道。 */
async function initSubtitleForItem(itemId, target = subTarget) {
  resetSubtitleLocal(target);
  const ctx = subCtx(target);
  try { ctx.info = await invoke('subtitle_info', { slot: target }); } catch (err) { ctx.info = null; }
  try { ctx.tracks = (await invoke('subtitle_tracks', { itemId })) || []; } catch (err) { ctx.tracks = []; }
  try { ctx.entryVisible = !!(await invoke('subtitle_entry_visible', { itemId, slot: target })); } catch (err) { ctx.entryVisible = false; }
  const reuse = pickReuseTrack(ctx.tracks);
  ctx.reuseTrackId = reuse ? reuse.id : null;
  ctx.reuseTable = reuse ? buildReuseTable(reuse) : [];
  ctx.reuseIndex = -1;
  ctx.reuseLine = null;
  renderSubtitleLines(target);
  if (target === subTarget) refreshSubtitleUi(target);
}

async function refreshSubtitleTracks(target = subTarget) {
  const ctx = subCtx(target);
  const itemId = subItemId(target);
  if (!itemId) return;
  try { ctx.tracks = (await invoke('subtitle_tracks', { itemId })) || []; } catch (err) { return; }
  const reuse = pickReuseTrack(ctx.tracks);
  ctx.reuseTrackId = reuse ? reuse.id : null;
  ctx.reuseTable = reuse ? buildReuseTable(reuse) : [];
  ctx.reuseIndex = -1;
  ctx.reuseLine = null;
  try { ctx.entryVisible = !!(await invoke('subtitle_entry_visible', { itemId, slot: target })); } catch (err) { /* 忽略 */ }
  renderSubtitleLines(target);
  if (target === subTarget) refreshSubtitleUi(target);
}

async function refreshSubtitleInfo(target = subTarget) {
  const ctx = subCtx(target);
  try { ctx.info = await invoke('subtitle_info', { slot: target }); } catch (err) { /* 忽略 */ }
  if (target === subTarget) refreshSubtitleUi(target);
}

/** 多轨并存时：外挂 > 实时生成（PRD §6 复用顺序）。 */
function pickReuseTrack(tracks) {
  const list = Array.isArray(tracks) ? tracks : [];
  return list.find((t) => t.kind === 'external') || list.find((t) => t.kind === 'generated') || null;
}

/** 把轨道的句按 startMs 归并为「行」；同一时刻的多个句视为双语对的原文/译文。 */
function buildReuseTable(track) {
  const cues = Array.isArray(track && track.cues) ? track.cues.slice() : [];
  if (!cues.length) return [];
  cues.sort((a, b) => (a.startMs || 0) - (b.startMs || 0));
  const rows = new Map();
  const table = [];
  for (const cue of cues) {
    const start = cue.startMs || 0;
    let row = rows.get(start);
    if (!row) {
      row = { startMs: start, endMs: cue.endMs || 0, texts: [] };
      rows.set(start, row);
      table.push(row);
    }
    if ((cue.endMs || 0) > row.endMs) row.endMs = cue.endMs || 0;
    if (cue.text) row.texts.push(cue.text);
  }
  return table.map((row) => ({
    startMs: row.startMs,
    endMs: row.endMs,
    origin: row.texts[0] || '',
    translation: row.texts.length > 1 ? row.texts[1] : '',
  }));
}

function renderReuseAt(positionMs, target = subTarget) {
  const ctx = subCtx(target);
  let idx = -1;
  for (let i = 0; i < ctx.reuseTable.length; i += 1) {
    const cue = ctx.reuseTable[i];
    if (positionMs >= cue.startMs && positionMs < cue.endMs) {
      idx = i;
      break;
    }
  }
  if (idx === ctx.reuseIndex) return;
  ctx.reuseIndex = idx;
  ctx.reuseLine = idx >= 0
    ? { origin: ctx.reuseTable[idx].origin, translation: ctx.reuseTable[idx].translation }
    : null;
  renderSubtitleLines(target);
}

function maybeSubtitleTick(positionMs, target = subTarget) {
  if (!playingId && !inMulti) return;
  const ctx = subCtx(target);
  if (liveOn(target)) {
    pollSubtitles(target);             // 实时识别：走后端轮询
  } else if (ctx.reuseTable.length) {
    renderReuseAt(positionMs, target); // 复用轨道：纯前端按进度渲染，无引擎参与
  }
}

async function pollSubtitles(target = subTarget) {
  const ctx = subCtx(target);
  if (ctx.polling) return;
  ctx.polling = true;
  try {
    const view = await invoke('subtitle_poll', { slot: target });
    applySubtitleView(view, target);
  } catch (err) {
    /* 轮询失败不打断播放 */
  } finally {
    ctx.polling = false;
  }
}

function applySubtitleView(view, target = subTarget) {
  if (!view) return;
  const ctx = subCtx(target);
  ctx.info = Object.assign({}, ctx.info || {}, view);
  if (Array.isArray(view.events)) applySubtitleEvents(view.events, target);
  // 失败弹层只针对当前菜单作用对象，避免后台格把弹层抢走
  if (view.state === 'failed' && target === subTarget) openSubtitleFailure(view.failure);
  // 质量低提示：同一次会话只给一次非侵入 toast
  if ((view.quality === 'degraded' || view.quality === 'critical') && !ctx.qualityNotified) {
    ctx.qualityNotified = true;
    showToast(view.quality === 'critical' ? '识别质量严重下降，字幕已降级' : '识别质量下降，字幕已降级');
  }
  renderSubtitleLines(target);
  if (target === subTarget) refreshSubtitleUi(target);
}

function applySubtitleEvents(events, target = subTarget) {
  const ctx = subCtx(target);
  for (const ev of events) {
    if (!ev) continue;
    switch (ev.kind) {
      case 'partial':
        // partial 只作占位弱化显示，不进入已保存轨道的计数
        ctx.lastPartial = ev.text || '';
        break;
      case 'cue':
        ctx.lastPartial = '';
        if (ev.isTranslation) ctx.lastTranslation = ev.text || '';
        else ctx.lastOrigin = ev.text || '';
        break;
      case 'degraded':
        if (ev.action === 'pause') {
          ctx.degradedNotice = '字幕已暂停以保护性能';
          showToast('字幕已暂停以保护性能');
        } else {
          ctx.degradedNotice = '识别队列过高，已降级为仅原文';
          showToast('识别队列过高，已降级为仅原文');
        }
        break;
      case 'failed':
        if (target === subTarget) openSubtitleFailure(ev.reason || ev.text);
        break;
      default:
        break; // quality 事件由 view.quality 的统一提示覆盖
    }
  }
}

/** 由上下文状态推导该帧的两行显示文本（原文 / 译文）。 */
function subtitleLinesOf(ctx, mode) {
  const reuse = ctx.reuseLine;
  const origin = reuse ? reuse.origin || '' : ctx.lastOrigin;
  const translation = reuse ? reuse.translation || '' : ctx.lastTranslation;
  const partial = reuse ? '' : ctx.lastPartial;

  const showOrigin = mode !== 'translationOnly';
  const showTrans = mode !== 'originOnly';

  let originText = '';
  let originPartial = false;
  let transText = '';
  let transPartial = false;
  if (showOrigin) {
    originText = origin || partial;
    originPartial = !origin && !!partial;
  }
  if (showTrans) {
    transText = translation;
    if (!transText && !showOrigin && partial) {
      transText = partial;
      transPartial = true;
    }
  }
  return { showOrigin, originText, originPartial, showTrans, transText, transPartial };
}

/** 单路播放的字幕叠层（贴在控制栏上方）。 */
function renderSubtitleOverlay(display, target = subTarget) {
  const ctx = subCtx(target);
  const mode = display || (ctx.info && ctx.info.display) || 'bilingual';
  const lines = subtitleLinesOf(ctx, mode);
  updateSubLine(els['sub-origin'], lines.originText, lines.originPartial, lines.showOrigin);
  updateSubLine(els['sub-translation'], lines.transText, lines.transPartial, lines.showTrans);
  const empty = !lines.originText && !lines.transText;
  els['subtitle-overlay'].classList.toggle('hidden', empty || !playbackActive());
}

/**
 * 格内字幕行：画在画面区与格内控制条之间。
 * 原生画面窗口只精确覆盖画面区，因此这一层始终可见。
 */
function renderCellSubtitle(slot, target = slotSubTarget(slot)) {
  const cell = els['multi-stage'].querySelector(`.screen-cell[data-slot="${slot}"]`);
  if (!cell) return;
  const origin = cell.querySelector('.cs-origin');
  const translation = cell.querySelector('.cs-translation');
  const ctx = subCtx(target);
  const mode = (ctx.info && ctx.info.display) || 'bilingual';
  const lines = subtitleLinesOf(ctx, mode);
  updateSubLine(origin, lines.originText, lines.originPartial, lines.showOrigin);
  updateSubLine(translation, lines.transText, lines.transPartial, lines.showTrans);
}

/** 清掉某格的字幕行（该格当前无字幕上下文时调用）。 */
function clearCellSubtitle(slot) {
  const cell = els['multi-stage'].querySelector(`.screen-cell[data-slot="${slot}"]`);
  if (!cell) return;
  updateSubLine(cell.querySelector('.cs-origin'), '', false, false);
  updateSubLine(cell.querySelector('.cs-translation'), '', false, false);
}

/**
 * 把某作用对象的两行字幕渲染到该去的地方：
 * - 主上下文（`null`）：单路播放走控制栏叠层；多屏时画在发声格内（后端会话绑在发声窗口）；
 * - 格上下文（屏位号）：画在该格内。
 */
function renderSubtitleLines(target = subTarget) {
  if (target == null) {
    const speaker = multiState ? multiState.speaker : null;
    if (inMulti && !(multiState && multiState.concurrentAudio) && speaker != null) {
      renderCellSubtitle(speaker, null);
    } else {
      renderSubtitleOverlay(undefined, null);
    }
    return;
  }
  renderCellSubtitle(target, target);
}

/** 是否存在正在播放的画面（单路播放，或多屏播放且未切到浏览库）。 */
function playbackActive() {
  return !!playingId || (inMulti && !browsingLibrary);
}

/** 幂等更新单行：只在状态变化时动 DOM。 */
function updateSubLine(node, text, partial, visible) {
  if (!node) return;
  const show = !!(visible && text);
  if (node.classList.contains('hidden') === !!show) {
    node.classList.toggle('hidden', !show);
  }
  if (node.classList.contains('partial') !== !!partial) {
    node.classList.toggle('partial', !!partial);
  }
  const value = show ? text : '';
  if (node.textContent !== value) {
    node.textContent = value;
  }
}

/** 清掉某作用对象的字幕行（只清 DOM，不改会话状态）。 */
function clearSubtitleLines(target = subTarget) {
  if (target == null) {
    hideAllSubtitleLines();
    return;
  }
  clearCellSubtitle(target);
}

/** 隐藏全部字幕行：进入浏览库 / 切换发声窗口 / 收起播放页时用。 */
function hideAllSubtitleLines() {
  hideSubtitleOverlay();
  if (multiState) {
    for (const view of multiState.slots) clearCellSubtitle(view.slot);
  }
}

function hideSubtitleOverlay() {
  els['subtitle-overlay'].classList.add('hidden');
  els['sub-origin'].classList.add('hidden');
  els['sub-translation'].classList.add('hidden');
  els['sub-origin'].textContent = '';
  els['sub-translation'].textContent = '';
}

function refreshSubtitleUi(target = subTarget) {
  const ctx = subCtx(target);
  // 「实时字幕」入口：仅当该条目可见入口且尚未开启时出现在控制栏（单路播放）
  els['pb-sublive'].classList.toggle('hidden', !(playingId && ctx.entryVisible) || liveOn(target));

  const state = ctx.info ? ctx.info.state : 'idle';
  if (ctx.info && (ctx.info.enabled || state !== 'idle')) {
    const count = typeof ctx.info.cueCount === 'number' ? ctx.info.cueCount : 0;
    els['sm-state'].textContent = `${SUB_STATE_TEXT[state] || '—'} · 已采集 ${count} 句`;
  } else {
    els['sm-state'].textContent = '未开启';
  }

  renderSubtitleNotice(target);
  renderSubtitleTracks(target);

  const on = liveOn(target);
  els['sm-start'].classList.toggle('hidden', on);
  els['sm-save'].classList.toggle('hidden', !on);
  els['sm-stop'].classList.toggle('hidden', !on && !(ctx.info && ctx.info.enabled));

  // 叠层以 display 为准，这里同步菜单里的呈现选择
  if (on && ctx.info && ctx.info.display) setDisplaySeg(ctx.info.display);
}

function renderSubtitleNotice(target = subTarget) {
  const ctx = subCtx(target);
  const queue = queueText(target);
  els['sm-queue'].textContent = queue;
  els['sm-queue'].classList.toggle('hidden', !queue);

  const notes = [];
  if (ctx.info && ctx.info.failure) notes.push(`失败原因：${ctx.info.failure}`);
  if (ctx.degradedNotice) notes.push(ctx.degradedNotice);
  els['sm-notice'].replaceChildren();
  els['sm-notice'].classList.toggle('hidden', notes.length === 0);
  for (const note of notes) els['sm-notice'].appendChild(el('div', 'sm-note', note));
}

/** 「识别队列」占用：由 quality 与 pausedForResources 推导（后端不单列该字段）。 */
function queueText(target = subTarget) {
  const ctx = subCtx(target);
  if (!ctx.info || !ctx.info.enabled) return '';
  if (ctx.info.pausedForResources) return '识别队列：保护性暂停';
  if (ctx.info.quality === 'critical') return '识别队列：占用过高';
  if (ctx.info.quality === 'degraded') return '识别队列：占用偏高';
  return '识别队列：正常';
}

function renderSubtitleTracks(target = subTarget) {
  const ctx = subCtx(target);
  els['sm-tracks'].replaceChildren();
  if (!ctx.tracks.length) {
    els['sm-tracks'].classList.add('hidden');
    return;
  }
  els['sm-tracks'].classList.remove('hidden');
  els['sm-tracks'].appendChild(el('div', 'sm-sub-title', '字幕轨道'));
  for (const track of ctx.tracks) {
    const row = el('div', 'sm-track');
    if (track.id === ctx.reuseTrackId && !liveOn(target)) row.classList.add('on');
    row.appendChild(el('span', 'st-kind', SUB_KIND_TEXT[track.kind] || '字幕'));
    if (track.lang) row.appendChild(el('span', 'st-lang', track.lang));
    if (typeof track.cueCount === 'number') row.appendChild(el('span', 'st-cnt', `${track.cueCount} 句`));
    els['sm-tracks'].appendChild(row);
  }
}

function openSubtitleMenu() {
  els['subtitle-menu'].classList.remove('hidden');
  refreshSubtitleUi(subTarget);
}

function closeSubtitleMenu() {
  els['subtitle-menu'].classList.add('hidden');
}

function hideSubtitleUi() {
  closeSubtitleMenu();
  els['subtitle-privacy'].classList.add('hidden');
  els['subtitle-failure'].classList.add('hidden');
  els['subtitle-external'].classList.add('hidden');
}

// ---- 隐私告知（第一次开启前必弹） ----

function openSubtitlePrivacy() {
  const ctx = subCtx();
  const engines = (ctx.info && ctx.info.engines) || [];
  els['sub-engine-list'].replaceChildren();

  if (!engines.length) {
    ctx.engineId = null;
    ctx.engineLocality = 'local';
    els['sub-engine-list'].appendChild(
      el('div', 'empty-sub', '未取得可用引擎清单，将使用默认本地识别（完全离线）。'),
    );
  } else {
    const pick = engines.find((e) => e.locality === 'local' && e.authorized)
      || engines.find((e) => e.authorized)
      || engines[0];
    ctx.engineId = pick.id;
    ctx.engineLocality = pick.locality || 'local';
    for (const engine of engines) {
      const usable = engine.locality !== 'remote' || engine.authorized;
      const btn = el('button', 'engine-opt' + (engine.id === ctx.engineId ? ' on' : ''));
      btn.type = 'button';
      btn.appendChild(el('span', 'eo-name', `${engine.locality === 'remote' ? '远端引擎' : '本地引擎'} · ${engine.id}`));
      btn.appendChild(el('span', 'eo-desc', engine.locality === 'remote'
        ? (engine.authorized ? '音频会上传到远端服务' : '未授权，无法使用')
        : '完全离线，音频不会离开本机'));
      btn.disabled = !usable;
      if (!usable) btn.title = '远端引擎未授权，可改用仅本地模式';
      btn.addEventListener('click', () => {
        ctx.engineId = engine.id;
        ctx.engineLocality = engine.locality || 'local';
        for (const other of els['sub-engine-list'].querySelectorAll('.engine-opt')) {
          other.classList.remove('on');
        }
        btn.classList.add('on');
        updatePrivacyNote();
      });
      els['sub-engine-list'].appendChild(btn);
    }
  }
  updatePrivacyNote();
  els['subtitle-privacy'].classList.remove('hidden');
}

/** 明确告知音频是否会离开本机；未授权远端时说明本次仅本地模式。 */
function updatePrivacyNote() {
  const ctx = subCtx();
  const remote = ctx.engineLocality === 'remote';
  const noRemoteAuth = !(ctx.info && ctx.info.remoteAuthorized);
  let text = remote
    ? '音频会离开本机：所选远端引擎会把音频上传到远端服务。'
    : '仅本地模式：本地识别完全离线，音频不会离开本机。';
  if (noRemoteAuth) text += '远端引擎未授权，本次仅本地模式。';
  els['sub-privacy-note'].textContent = text;
}

function hideSubtitlePrivacy() {
  els['subtitle-privacy'].classList.add('hidden');
}

async function confirmSubtitlePrivacy() {
  hideSubtitlePrivacy();
  const ctx = subCtx();
  const allowRemote = ctx.engineLocality === 'remote';
  try {
    ctx.info = await invoke('subtitle_set_enabled', { enabled: true, allowRemote, slot: subTarget });
  } catch (err) {
    showToast(String(err));
    return;
  }
  await startLive();
}

// ---- 呈现与生成 ----

function currentDisplay() {
  const on = els['sm-display-group'].querySelector('.seg-btn.on');
  return on ? on.dataset.display : 'bilingual';
}

function setDisplaySeg(display) {
  for (const btn of els['sm-display-group'].querySelectorAll('.seg-btn')) {
    btn.classList.toggle('on', btn.dataset.display === display);
  }
}

async function applyDisplay(display) {
  const ctx = subCtx();
  if (liveOn(subTarget)) {
    try {
      await invoke('subtitle_set_display', { display, slot: subTarget });
    } catch (err) {
      showToast(String(err));
    }
    if (ctx.info) ctx.info.display = display;
  }
  renderSubtitleLines(subTarget);
}

async function requestStartLive() {
  // 第一次开启前必须先过隐私告知
  const ctx = subCtx();
  if (!ctx.info || !ctx.info.enabled) {
    openSubtitlePrivacy();
    return;
  }
  await startLive();
}

async function startLive() {
  const ctx = subCtx();
  const originLang = els['sm-origin-lang'].value || null;
  const targetLang = els['sm-target-lang'].value || null;
  const display = currentDisplay();
  const engineId = ctx.engineId || null;
  try {
    await invoke('subtitle_start', { originLang, targetLang, display, engineId, slot: subTarget });
  } catch (err) {
    showToast(String(err));
    return;
  }
  resetSubtitleText(subTarget);
  ctx.qualityNotified = false;
  ctx.degradedNotice = '';
  // 实时字幕接管后，不再渲染复用的外挂/生成轨道
  ctx.reuseTable = [];
  ctx.reuseLine = null;
  ctx.reuseIndex = -1;
  hideSubtitleFailure();
  await refreshSubtitleInfo(subTarget);
  showToast('已开始实时字幕');
}

/** 收口实时字幕；返回 subtitle_close 结果（saved 表示有未成句已自动落库）。 */
async function closeSubtitleSession(target = subTarget) {
  let result = null;
  const ctx = subCtx(target);
  if (ctx.info && ctx.info.enabled) {
    try { result = await invoke('subtitle_close', { slot: target }); } catch (err) { result = null; }
  }
  if (result && result.saved) {
    showToast('已把未保存的字幕保存为轨道');
  }
  resetSubtitleLocal(target);
  clearSubtitleLines(target);
  return result;
}

async function stopLive() {
  const had = liveOn(subTarget) || !!((subCtx().info || {}).enabled);
  const result = await closeSubtitleSession();
  hideSubtitleFailure();
  refreshSubtitleUi(subTarget);
  if (had && !(result && result.saved)) showToast('已关闭实时字幕');
}

async function saveSubtitleTrack() {
  let result;
  try {
    result = await invoke('subtitle_save', { slot: subTarget });
  } catch (err) {
    showToast(String(err));
    return;
  }
  const count = result && typeof result.cueCount === 'number' ? result.cueCount : 0;
  showToast(`已保存为字幕轨道（${count} 句），下次播放自动复用`);
  await refreshSubtitleInfo(subTarget);
  await refreshSubtitleTracks(subTarget);
}

// ---- 失败三出口 ----

function openSubtitleFailure(reason) {
  if (!els['subtitle-failure'].classList.contains('hidden')) return;
  els['sub-failure-desc'].textContent = reason ? `原因：${reason}` : '实时字幕生成失败。';
  els['subtitle-failure'].classList.remove('hidden');
  clearSubtitleLines(subTarget);
}

function hideSubtitleFailure() {
  els['subtitle-failure'].classList.add('hidden');
}

/** 出口二：仅做原文听写（保留已选语种/引擎，display 固定 originOnly）。 */
async function retryOriginOnly() {
  hideSubtitleFailure();
  setDisplaySeg('originOnly');
  const ctx = subCtx();
  if (!ctx.info || !ctx.info.enabled) {
    openSubtitlePrivacy();
    return;
  }
  await startLive();
}

async function closeLiveFromFailure() {
  hideSubtitleFailure();
  const result = await closeSubtitleSession();
  refreshSubtitleUi(subTarget);
  if (!(result && result.saved)) showToast('已关闭实时字幕');
}

// ---- 外挂字幕（失败出口一 / 复用） ----

function openExternalModal() {
  els['sub-external-path'].value = '';
  els['subtitle-external'].classList.remove('hidden');
  els['sub-external-path'].focus();
}

function hideExternalModal() {
  els['subtitle-external'].classList.add('hidden');
}

async function confirmExternal() {
  const path = els['sub-external-path'].value.trim();
  if (!path) {
    showToast('请填写字幕文件绝对路径');
    return;
  }
  try {
    await invoke('subtitle_add_external', { path, slot: subTarget });
  } catch (err) {
    showToast(String(err));
    return;
  }
  hideExternalModal();
  await refreshSubtitleTracks(subTarget); // 添加后切到外挂轨道渲染
  showToast('已加载外挂字幕');
}

function bindSubtitleEvents() {
  els['pb-subtitle'].addEventListener('click', () => {
    // 单路播放控制栏上的字幕菜单：作用于主上下文
    subTarget = null;
    if (els['subtitle-menu'].classList.contains('hidden')) openSubtitleMenu();
    else closeSubtitleMenu();
  });
  els['pb-sublive'].addEventListener('click', () => {
    subTarget = null;
    openSubtitleMenu();
    requestStartLive();
  });
  els['sm-close'].addEventListener('click', closeSubtitleMenu);

  els['sm-start'].addEventListener('click', requestStartLive);
  els['sm-save'].addEventListener('click', saveSubtitleTrack);
  els['sm-stop'].addEventListener('click', stopLive);

  els['sm-origin-lang'].addEventListener('change', () => {
    if (liveOn(subTarget)) showToast('原声语种将在下次生成时生效');
  });
  els['sm-target-lang'].addEventListener('change', () => {
    // 「不翻译」（原声同语种）只显示原文，等价于 display=originOnly
    const noTranslate = els['sm-target-lang'].value === '';
    setDisplaySeg(noTranslate ? 'originOnly' : 'bilingual');
    applyDisplay(currentDisplay());
  });
  els['sm-display-group'].addEventListener('click', (event) => {
    const btn = event.target.closest('.seg-btn');
    if (!btn) return;
    if (els['sm-target-lang'].value === '' && btn.dataset.display !== 'originOnly') {
      showToast('目标语言为「不翻译」，仅显示原文');
      setDisplaySeg('originOnly');
      applyDisplay('originOnly');
      return;
    }
    setDisplaySeg(btn.dataset.display);
    applyDisplay(btn.dataset.display);
  });

  els['sub-privacy-confirm'].addEventListener('click', confirmSubtitlePrivacy);
  els['sub-privacy-cancel'].addEventListener('click', hideSubtitlePrivacy);

  els['sub-fail-external'].addEventListener('click', () => {
    hideSubtitleFailure();
    openExternalModal();
  });
  els['sub-fail-dictation'].addEventListener('click', retryOriginOnly);
  els['sub-fail-close'].addEventListener('click', closeLiveFromFailure);

  els['sub-external-confirm'].addEventListener('click', confirmExternal);
  els['sub-external-cancel'].addEventListener('click', hideExternalModal);
  els['sub-external-path'].addEventListener('keydown', (event) => {
    if (event.key === 'Enter') confirmExternal();
  });
}

// ---------------------------------------------------------------------------
// 多屏播放（FR-06）
// ---------------------------------------------------------------------------
// 前端只做「呈现 + 转发」：格位数与上限由后端给出，屏位归属、发声切换、过载与
// 退出策略全部在后端收口。原生画面窗口覆盖每格的 .screen-area，因此格内所有可点
// 控件都留在画面区之外的顶部条与控制条里。

/** 最多同时选择的路数（与 P0 布局的最大格位数一致：2 × 2 = 4）。 */
const MULTI_MAX_SELECT = 4;

/** 按后端返回的布局渲染舞台与布局条。 */
async function loadLayouts() {
  try {
    multiLayouts = (await invoke('multi_layouts')) || [];
  } catch (err) {
    multiLayouts = [];
  }
  els['layout-bar'].replaceChildren();
  for (const layout of multiLayouts) {
    const btn = el('button', 'btn', layout.label);
    btn.type = 'button';
    btn.dataset.layout = layout.tag;
    btn.title = `${layout.label} · ${layout.slots} 个格位`;
    btn.addEventListener('click', () => changeLayout(layout.tag));
    els['layout-bar'].appendChild(btn);
  }
}

function slotsOfLayout(tag) {
  const found = multiLayouts.find((l) => l.tag === tag);
  return found ? found.slots : 2;
}

function itemById(id) {
  return items.find((i) => i.id === id);
}

// ---- 多选态 ----

function toggleSelect(item) {
  if (item.missing) {
    showToast('文件缺失，无法播放');
    return;
  }
  const at = selectedIds.indexOf(item.id);
  if (at >= 0) {
    selectedIds.splice(at, 1);
  } else {
    if (selectedIds.length >= MULTI_MAX_SELECT) {
      showToast(`最多同时播放 ${MULTI_MAX_SELECT} 路，请先取消部分选择`);
      return;
    }
    selectedIds.push(item.id);
  }
  renderGrid();
  renderSelectBar();
}

function renderSelectBar() {
  els['select-count'].textContent = `已选 ${selectedIds.length} 部`;
  els['select-enter'].disabled = selectedIds.length < 2;
  els['multi-toggle'].classList.toggle('primary', selectMode);
}

function enterSelectMode(preselectId) {
  selectMode = true;
  selectedIds = [];
  if (preselectId) {
    selectedIds.push(preselectId);
  }
  document.body.classList.add('selecting');
  els['select-bar'].classList.remove('hidden');
  renderGrid();
  renderSelectBar();
}

function exitSelectMode() {
  selectMode = false;
  selectedIds = [];
  document.body.classList.remove('selecting');
  els['select-bar'].classList.add('hidden');
  renderGrid();
  renderSelectBar();
}

function toggleSelectMode() {
  if (consoleOn) {
    showToast('主机模式不提供多屏播放，请先按 Esc 回到经典界面');
    return;
  }
  if (inMulti) {
    showToast('已在多屏播放中，可用「退出多屏」返回');
    return;
  }
  if (selectMode) {
    exitSelectMode();
  } else {
    enterSelectMode(null);
  }
}

// ---- 视图切换 ----

/** 按当前状态切换「经典界面 / 主机模式 / 多屏舞台」的可见性；不改变任何后端状态。 */
function applyView() {
  const stageOn = inMulti && !browsingLibrary;
  const consoleMode = consoleShown();
  // 主机层与经典界面互斥：显示主机层时整层隐藏经典 chrome（播放中主机层不可见）
  els['console'].classList.toggle('hidden', !consoleMode);
  els.topbar.classList.toggle('hidden', consoleMode);
  els.searchbar.classList.toggle('hidden', stageOn || consoleMode);
  els.wall.classList.toggle('hidden', stageOn || consoleMode);
  els['multi-stage'].classList.toggle('hidden', !stageOn);
  els['multi-bar'].classList.toggle('hidden', !inMulti || consoleMode);
  els['multi-toggle'].classList.toggle('hidden', inMulti);
  els.sources.classList.toggle('hidden', stageOn || !!playingId || consoleMode);
  if (stageOn || consoleMode) {
    els.dims.classList.add('hidden');
    els['filter-bar'].classList.add('hidden');
  } else {
    els.dims.classList.toggle('hidden', !dimsOpen);
    els['filter-bar'].classList.toggle('hidden', activeEntries().length === 0);
  }
}

// ---- 舞台与格位 ----

/** 舞台每格 DOM：顶部条（格位 + 发声切换）、画面区、控制条、错误/空位卡。 */
function buildCell(slot) {
  const cell = el('div', 'screen-cell');
  cell.dataset.slot = String(slot);

  const top = el('div', 'cell-top');
  top.appendChild(el('span', 'cell-tag', `格 ${slot + 1}`));
  const flags = el('span', 'cell-flags');
  top.appendChild(flags);
  top.appendChild(el('div', 'grow'));
  const speaker = el('button', 'cell-speaker', '静音');
  speaker.type = 'button';
  speaker.addEventListener('click', () => setSpeaker(slot));
  top.appendChild(speaker);
  cell.appendChild(top);

  cell.appendChild(el('div', 'screen-area'));

  // 格内字幕行：位于画面区与格内控制条之间。原生画面窗口只覆盖画面区，
  // 因此这一层始终可见（并发发声模式下每格各自显示，与后端每格会话对应）。
  const subLayer = el('div', 'cell-sub');
  subLayer.appendChild(el('div', 'cs-line cs-origin hidden'));
  subLayer.appendChild(el('div', 'cs-line cs-translation hidden'));
  cell.appendChild(subLayer);

  const bar = el('div', 'cell-bar');
  bar.appendChild(el('span', 'cb-title', '—'));
  const toggle = el('button', 'cb-btn cb-toggle', '暂停');
  toggle.type = 'button';
  toggle.addEventListener('click', () => toggleSlotPause(slot));
  bar.appendChild(toggle);
  const seek = document.createElement('input');
  seek.type = 'range';
  seek.className = 'cb-seek';
  seek.min = '0';
  seek.max = '1000';
  seek.step = '1';
  seek.value = '0';
  seek.addEventListener('input', () => draggingSlots.add(slot));
  seek.addEventListener('change', () => onSlotSeek(slot, seek));
  bar.appendChild(seek);
  bar.appendChild(el('span', 'cb-time', '00:00 / 00:00'));
  const rate = document.createElement('select');
  rate.className = 'cb-rate';
  for (const value of ['0.5', '0.75', '1', '1.25', '1.5', '2']) {
    const opt = document.createElement('option');
    opt.value = value;
    opt.textContent = `${value}×`;
    rate.appendChild(opt);
  }
  rate.value = '1';
  rate.addEventListener('change', () => setSlotRate(slot, rate.value));
  bar.appendChild(rate);
  const audio = document.createElement('select');
  audio.className = 'cb-audio';
  audio.addEventListener('change', () => setSlotAudio(slot, audio.value));
  bar.appendChild(audio);
  const sub = el('button', 'cb-btn cb-sub', '字幕');
  sub.type = 'button';
  sub.addEventListener('click', () => openSlotSubtitle(slot));
  bar.appendChild(sub);
  const close = el('button', 'cb-btn cb-close', '关闭');
  close.type = 'button';
  close.addEventListener('click', () => removeSlot(slot));
  bar.appendChild(close);
  cell.appendChild(bar);

  cell.appendChild(el('div', 'cell-card hidden'));
  return cell;
}

function renderStage(tag) {
  els['multi-stage'].dataset.layout = tag;
  els['multi-stage'].replaceChildren();
  for (let i = 0; i < slotsOfLayout(tag); i += 1) {
    els['multi-stage'].appendChild(buildCell(i));
  }
}

/** 某格画面区的物理像素矩形（原生画面窗口按此摆放）。 */
function rectOfSlot(slot) {
  const cell = els['multi-stage'].querySelector(`.screen-cell[data-slot="${slot}"]`);
  if (!cell) return null;
  const area = cell.querySelector('.screen-area');
  const rect = area.getBoundingClientRect();
  return {
    x: toPhysical(rect.left),
    y: toPhysical(rect.top),
    w: toPhysical(rect.width),
    h: toPhysical(rect.height),
  };
}

/** 按当前格位矩形重摆所有已开启的画面窗口（布局变化 / 窗口尺寸变化后调用）。 */
async function placeAllSlots() {
  if (!multiState || browsingLibrary) return;
  for (const view of multiState.slots) {
    if (view.fault) continue; // 故障格窗口已隐藏，保持错误卡可见
    const rect = rectOfSlot(view.slot);
    if (!rect || rect.w <= 0 || rect.h <= 0) continue;
    try {
      await invoke('multi_place', { slot: view.slot, x: rect.x, y: rect.y, width: rect.w, height: rect.h });
    } catch (err) {
      /* 单格重摆失败不影响其他格 */
    }
  }
}

/** 多屏心跳：拉整体状态（后端顺带落库每格进度），并刷新布局条与各格。 */
async function multiTick() {
  if (!inMulti) return;
  let view;
  try {
    view = await invoke('multi_status');
  } catch (err) {
    return;
  }
  multiState = view;
  renderMultiBar();
  if (!els['multi-stage'].classList.contains('hidden')) {
    renderMultiSlots();
  }
}

function renderMultiBar() {
  if (!multiState) return;
  for (const btn of els['layout-bar'].querySelectorAll('.btn')) {
    btn.classList.toggle('on', btn.dataset.layout === multiState.layout);
  }
  els['multi-count'].textContent = `${multiState.slots.length} 路 · 上限 ${multiState.limit}`;
  renderLimitOptions(multiState.limit);
  // 并发发声开关：回显后端持久化值（默认关闭，硬约束 C6）
  const concurrent = !!multiState.concurrentAudio;
  if (els['concurrent-audio'].checked !== concurrent) {
    els['concurrent-audio'].checked = concurrent;
  }
  els['browse-btn'].textContent = browsingLibrary ? '返回多屏' : '浏览库';
  els['browse-btn'].classList.toggle('primary', browsingLibrary);
}

/** 上限选项：至少到 4 路（P0 最大格位），已持久化更大的值也照实呈现。 */
function renderLimitOptions(limit) {
  const max = Math.max(4, limit);
  const sel = els['limit-select'];
  if (sel.dataset.max !== String(max)) {
    sel.replaceChildren();
    for (let n = 1; n <= max; n += 1) {
      const opt = document.createElement('option');
      opt.value = String(n);
      opt.textContent = `${n} 路`;
      sel.appendChild(opt);
    }
    sel.dataset.max = String(max);
  }
  sel.value = String(limit);
}

function renderMultiSlots() {
  const stage = els['multi-stage'];
  if (!multiState || stage.classList.contains('hidden')) return;
  for (const cell of stage.querySelectorAll('.screen-cell')) {
    const slot = Number(cell.dataset.slot);
    const view = multiState.slots.find((s) => s.slot === slot);
    if (view) {
      renderSlotCell(cell, view, multiState.speaker === slot);
    } else {
      renderEmptyCell(cell, slot);
    }
  }
  syncFaultWindows();
  // 字幕：并发发声模式下每格各自会话（多路同时识别与显示，互不干扰）；
  // 否则多屏只有发声窗口一路（会话在主上下文，与后端收口一致）。
  for (const view of multiState.slots) {
    const target = slotSubTarget(view.slot);
    if (target === undefined) {
      clearCellSubtitle(view.slot);
      continue;
    }
    maybeSubtitleTick(view.positionMs || 0, target);
    renderCellSubtitle(view.slot, target);
  }
}

/** 故障格：隐藏其原生画面窗口，让错误卡可见（每格只做一次）。 */
function syncFaultWindows() {
  for (const view of multiState.slots) {
    if (view.fault) {
      if (!faultHidden.has(view.slot)) {
        faultHidden.add(view.slot);
        invoke('multi_hide_slot', { slot: view.slot }).catch(() => {});
      }
    } else {
      faultHidden.delete(view.slot);
    }
  }
}

function actionBtn(label, handler) {
  const btn = el('button', 'btn', label);
  btn.type = 'button';
  btn.addEventListener('click', handler);
  return btn;
}

function renderSlotCell(cell, view, isSpeaker) {
  const flagText = view.fault
    ? '故障'
    : view.buffering
      ? '缓冲中'
      : view.ended
        ? '已结束'
        : '';
  const flags = cell.querySelector('.cell-flags');
  if (flags.textContent !== flagText) {
    flags.textContent = flagText;
  }

  const concurrent = !!(multiState && multiState.concurrentAudio);
  const speakerBtn = cell.querySelector('.cell-speaker');
  // 并发发声：各格都在出声，「发声切换」退化为单条命令的作用对象，故只作状态标注
  const speakerText = concurrent
    ? '多路同时发声'
    : (isSpeaker ? '当前发声' : '静音 · 点此发声');
  if (speakerBtn.textContent !== speakerText) {
    speakerBtn.textContent = speakerText;
  }
  speakerBtn.classList.toggle('on', concurrent || isSpeaker);
  speakerBtn.disabled = concurrent || isSpeaker;

  // 故障卡 / 正常：只在模式或文案变化时重建节点，避免每帧抖动
  const card = cell.querySelector('.cell-card');
  if (view.fault) {
    const text = `该格播放失败：${view.fault}`;
    if (card.dataset.mode !== 'fault' || card.dataset.text !== text) {
      card.dataset.mode = 'fault';
      card.dataset.text = text;
      card.replaceChildren();
      card.appendChild(el('div', 'cc-text fault', text));
      const actions = el('div', 'cc-actions');
      actions.appendChild(actionBtn('重试', () => retrySlot(view.slot)));
      actions.appendChild(actionBtn('更换影片', () => changeSlotFilm(view.slot)));
      card.appendChild(actions);
    }
    card.classList.remove('hidden');
  } else if (card.dataset.mode !== 'none') {
    card.dataset.mode = 'none';
    card.dataset.text = '';
    card.replaceChildren();
    card.classList.add('hidden');
  }

  const title = cell.querySelector('.cb-title');
  const titleText = view.title || '—';
  if (title.textContent !== titleText) {
    title.textContent = titleText;
  }

  const off = !view.playing || !!view.fault;
  const toggle = cell.querySelector('.cb-toggle');
  const toggleText = pausedSlots.has(view.slot) ? '播放' : '暂停';
  if (toggle.textContent !== toggleText) {
    toggle.textContent = toggleText;
  }
  toggle.disabled = off;

  const seek = cell.querySelector('.cb-seek');
  const duration = view.durationMs || 0;
  if (!draggingSlots.has(view.slot) && duration > 0) {
    seek.value = String(Math.min(1000, Math.round((view.positionMs / duration) * 1000)));
  }
  seek.disabled = off;

  const time = cell.querySelector('.cb-time');
  const timeText = `${formatTime(view.positionMs)} / ${duration ? formatTime(duration) : '--:--'}`;
  if (time.textContent !== timeText) {
    time.textContent = timeText;
  }

  const rate = cell.querySelector('.cb-rate');
  rate.value = String(rateOf[view.slot] || 1);
  rate.disabled = off;

  // 字幕入口：并发发声模式下每格各有一份会话，全部可用；
  // 否则只有发声窗口可用（会话绑在该格，PRD FR-05 多屏联动）
  const sub = cell.querySelector('.cb-sub');
  const subUsable = concurrent || isSpeaker;
  sub.disabled = off || !subUsable;
  sub.title = subUsable ? '实时字幕菜单' : '未开启「多路同时发声」时，仅发声窗口可用实时字幕';

  // 音轨：索引与文案来自后端探测的流事实（按格缓存，换片时才重取）
  syncAudioTracks(view.slot, view.itemId, cell.querySelector('.cb-audio'), off);
}

function renderEmptyCell(cell, slot) {
  const flags = cell.querySelector('.cell-flags');
  if (flags.textContent !== '空位') {
    flags.textContent = '空位';
  }
  const speakerBtn = cell.querySelector('.cell-speaker');
  speakerBtn.textContent = '静音';
  speakerBtn.classList.remove('on');
  speakerBtn.disabled = true;

  const card = cell.querySelector('.cell-card');
  if (card.dataset.mode !== 'empty') {
    card.dataset.mode = 'empty';
    card.dataset.text = '';
    card.replaceChildren();
    card.appendChild(el('div', 'cc-text', '空位'));
    const actions = el('div', 'cc-actions');
    actions.appendChild(actionBtn('添加影片', () => changeSlotFilm(slot)));
    card.appendChild(actions);
  }
  card.classList.remove('hidden');

  cell.querySelector('.cb-title').textContent = '空位';
  cell.querySelector('.cb-toggle').disabled = true;
  const seek = cell.querySelector('.cb-seek');
  seek.disabled = true;
  seek.value = '0';
  cell.querySelector('.cb-time').textContent = '00:00 / --:--';
  cell.querySelector('.cb-rate').disabled = true;
  const audio = cell.querySelector('.cb-audio');
  audio.replaceChildren();
  audio.disabled = true;
  cell.querySelector('.cb-sub').disabled = true;
  cell.querySelector('.cb-close').disabled = true;
  clearCellSubtitle(slot);
}

async function syncAudioTracks(slot, itemId, select, off) {
  const cached = audioCache[slot];
  if (!cached || cached.itemId !== itemId) {
    let tracks = [];
    try {
      tracks = (await invoke('multi_audio_tracks', { slot })) || [];
    } catch (err) {
      tracks = [];
    }
    audioCache[slot] = { itemId, tracks, selected: null };
    select.replaceChildren();
    if (!tracks.length) {
      const opt = document.createElement('option');
      opt.value = '';
      opt.textContent = '默认音轨';
      select.appendChild(opt);
    } else {
      for (const track of tracks) {
        const opt = document.createElement('option');
        opt.value = String(track.index);
        opt.textContent = track.label || `音轨 ${track.index + 1}`;
        select.appendChild(opt);
      }
    }
  }
  const entry = audioCache[slot];
  select.value = entry.selected != null
    ? String(entry.selected)
    : (select.options.length ? select.options[0].value : '');
  select.disabled = off || !entry.tracks.length;
}

// ---- 进入 / 退出多屏 ----

async function enterMulti() {
  if (consoleOn) {
    showToast('主机模式不提供多屏播放，请先按 Esc 回到经典界面');
    return;
  }
  if (multiBusy || inMulti || selectedIds.length < 2) {
    return;
  }
  multiBusy = true;
  try {
    const tag = selectedIds.length > 2 ? '2x2' : '1x2';
    let view;
    try {
      view = await invoke('multi_set_layout', { layout: tag });
    } catch (err) {
      showToast(String(err));
      return;
    }

    selectMode = false;
    document.body.classList.remove('selecting');
    els['select-bar'].classList.add('hidden');
    inMulti = true;
    browsingLibrary = false;
    pendingSlot = null;
    pausedSlots.clear();
    draggingSlots.clear();
    faultHidden.clear();
    for (const key of Object.keys(audioCache)) {
      delete audioCache[key];
    }
    stopPreview();
    hideResume();
    // 进入多屏：字幕作用对象重来（单路主上下文已随单路播放收尾）
    resetAllSubtitleContexts();

    multiState = view;
    els['multi-bar'].classList.remove('hidden');
    els['player-bar'].classList.add('hidden');
    applyView();
    renderStage(view.layout);
    renderMultiBar();

    // 逐格打开：先量出格位矩形再交给后端摆放原生画面窗口
    let firstError = null;
    for (let i = 0; i < selectedIds.length; i += 1) {
      const rect = rectOfSlot(i);
      if (!rect) continue;
      try {
        multiState = await invoke('multi_open', {
          slot: i,
          itemId: selectedIds[i],
          x: rect.x,
          y: rect.y,
          width: rect.w,
          height: rect.h,
        });
      } catch (err) {
        if (!firstError) firstError = String(err);
      }
    }
    if (firstError) {
      showToast(firstError);
    }
    renderMultiBar();
    renderMultiSlots();
    startTick();
  } finally {
    multiBusy = false;
  }
}

function openMultiExit() {
  if (!inMulti) return;
  els['multi-exit-modal'].classList.remove('hidden');
}

function hideMultiExit() {
  els['multi-exit-modal'].classList.add('hidden');
}

/** 退出多屏：必须用户明确二选一（PRD FR-06「退出」），不设默认。 */
async function exitMulti(policy) {
  if (!inMulti) {
    hideMultiExit();
    return;
  }
  hideMultiExit();
  const speakerView = multiState && multiState.slots.find((s) => s.slot === multiState.speaker);
  const keepId = speakerView ? speakerView.itemId : null;
  const keepPos = speakerView ? speakerView.positionMs || 0 : 0;

  stopTick();
  stopPreview();
  inMulti = false;
  browsingLibrary = false;
  pendingSlot = null;
  pausedSlots.clear();
  draggingSlots.clear();
  faultHidden.clear();
  els['multi-bar'].classList.add('hidden');
  els['multi-stage'].classList.add('hidden');
  els['multi-stage'].replaceChildren();
  multiState = null;
  hideSubtitleUi();
  // 退出多屏后回到单路字幕上下文（后端已收尾全部会话）
  resetAllSubtitleContexts();

  try {
    await invoke('multi_exit', { policy });
  } catch (err) {
    showToast(String(err));
  }

  if (policy === 'keep_progress' && keepId) {
    // 保留发声窗口的进度并回到单路：以该位置重开 0 号屏位
    applyView();
    await play(keepId, keepPos, itemById(keepId));
  } else {
    applyView();
    await refresh();
  }
  // 退出多屏后若此前处于主机模式，让主机层按当前状态回归
  syncConsole();
}

// ---- 多屏期内的操作 ----

async function changeLayout(tag) {
  if (multiBusy || !inMulti || !multiState || multiState.layout === tag) return;
  multiBusy = true;
  try {
    let view;
    try {
      view = await invoke('multi_set_layout', { layout: tag });
    } catch (err) {
      showToast(String(err));
      return;
    }
    multiState = view;
    renderStage(view.layout);
    renderMultiBar();
    renderMultiSlots();
    if (!browsingLibrary) {
      await placeAllSlots();
    }
  } finally {
    multiBusy = false;
  }
}

async function setLimit(value) {
  const n = Number(value);
  if (!Number.isFinite(n) || n < 1) return;
  let view;
  try {
    view = await invoke('multi_set_limit', { limit: Math.round(n) });
  } catch (err) {
    showToast(String(err));
    return;
  }
  multiState = view;
  renderMultiBar();
}

/** 开关并发发声（默认关闭）：开启后多路同时出声，实时字幕随之放开到各格。 */
async function setConcurrentAudio(enabled) {
  let view;
  try {
    view = await invoke('multi_set_concurrent_audio', { enabled });
  } catch (err) {
    showToast(String(err));
    els['concurrent-audio'].checked = !enabled;
    return;
  }
  multiState = view;
  // 切换发声模式时后端会收尾旧模式的全部会话；本端同步清空字幕上下文与字幕行
  resetAllSubtitleContexts();
  hideAllSubtitleLines();
  refreshSubtitleUi(subTarget);
  renderMultiBar();
  renderMultiSlots();
  showToast(enabled
    ? '已切换为多路同时发声：各格可在格内单独开启实时字幕'
    : '已切回单路发声：各格实时字幕已停止');
}

async function setSpeaker(slot) {
  let view;
  try {
    view = await invoke('multi_set_speaker', { slot });
  } catch (err) {
    showToast(String(err));
    return;
  }
  multiState = view;
  // 并发发声模式下每格各有独立会话，切换作用对象不影响它们；单路发声时
  // 字幕抽头绑在旧发声窗口的主上下文上，切换后后端已收尾，这里同步清空本端
  if (!multiState.concurrentAudio) {
    resetSubtitleLocal(null);
    hideAllSubtitleLines();
    refreshSubtitleUi(subTarget);
  }
  renderMultiBar();
  renderMultiSlots();
}

async function toggleSlotPause(slot) {
  const paused = !pausedSlots.has(slot);
  if (paused) {
    pausedSlots.add(slot);
  } else {
    pausedSlots.delete(slot);
  }
  try {
    await invoke('multi_set_paused', { slot, paused });
  } catch (err) {
    showToast(String(err));
  }
  renderMultiSlots();
}

async function onSlotSeek(slot, seek) {
  draggingSlots.delete(slot);
  const view = multiState && multiState.slots.find((s) => s.slot === slot);
  const duration = view && view.durationMs;
  if (!duration) return;
  const positionMs = Math.round((Number(seek.value) / 1000) * duration);
  try {
    await invoke('multi_seek', { slot, positionMs });
  } catch (err) {
    showToast(String(err));
  }
  const target = slotSubTarget(slot);
  if (target !== undefined) {
    resetSubtitleText(target);
    renderSubtitleLines(target);
  }
}

async function setSlotRate(slot, value) {
  rateOf[slot] = Number(value);
  try {
    await invoke('multi_set_rate', { slot, rate: Number(value) });
  } catch (err) {
    showToast(String(err));
  }
}

async function setSlotAudio(slot, value) {
  const index = Number(value);
  if (!Number.isFinite(index)) return;
  const entry = audioCache[slot];
  if (entry) {
    // 记住本端选择，轮询回显时保持一致
    entry.selected = index;
  }
  try {
    await invoke('multi_select_track', { slot, kind: 'audio', index });
  } catch (err) {
    showToast(String(err));
  }
}

async function openSlotSubtitle(slot) {
  const target = slotSubTarget(slot);
  if (target === undefined) {
    showToast('未开启「多路同时发声」时，仅发声窗口可用实时字幕');
    return;
  }
  const view = multiState.slots.find((s) => s.slot === slot);
  subTarget = target;
  if (view) {
    await initSubtitleForItem(view.itemId, target);
  }
  openSubtitleMenu();
}

async function removeSlot(slot) {
  let view;
  try {
    view = await invoke('multi_remove', { slot });
  } catch (err) {
    showToast(String(err));
    return;
  }
  multiState = view;
  pausedSlots.delete(slot);
  draggingSlots.delete(slot);
  faultHidden.delete(slot);
  delete audioCache[slot];
  delete rateOf[slot];
  // 该格已关闭：丢弃它的字幕上下文（后端同步收尾了会话）
  dropSlotSubtitleContext(slot);
  renderMultiBar();
  renderMultiSlots();
}

/** 丢弃某格的字幕上下文；若菜单正作用于该格，回到主上下文并收起菜单。 */
function dropSlotSubtitleContext(slot) {
  subContexts.delete(subKeyOf(slot));
  if (subTarget === slot) {
    subTarget = null;
    closeSubtitleMenu();
    refreshSubtitleUi(subTarget);
  }
}

async function retrySlot(slot) {
  const view = multiState && multiState.slots.find((s) => s.slot === slot);
  if (!view) return;
  await openSlotFilm(slot, view.itemId, view.positionMs || 0);
}

/** 「更换影片 / 空位添加」：先切换到浏览库，由用户挑一部填入待定屏位。 */
async function changeSlotFilm(slot) {
  pendingSlot = slot;
  if (!browsingLibrary) {
    browsingLibrary = true;
    await invoke('multi_hide_all').catch(() => {});
    hideAllSubtitleLines();
  }
  applyView();
  showToast(`请选择要放入「格 ${slot + 1}」的影片`);
}

/** 浏览库时点某张海报：填入待定屏位或当前布局的第一个空位。 */
async function addCardToMulti(item) {
  if (item.missing) {
    showToast('文件缺失，无法播放');
    return;
  }
  const slot = pendingSlot != null ? pendingSlot : firstEmptySlot();
  if (slot == null) {
    showToast('当前布局没有空位，请先关闭一格或调整布局');
    return;
  }
  pendingSlot = null;
  await openSlotFilm(slot, item.id, null);
}

function firstEmptySlot() {
  if (!multiState) return null;
  const open = new Set(multiState.slots.map((s) => s.slot));
  const total = slotsOfLayout(multiState.layout);
  for (let i = 0; i < total; i += 1) {
    if (!open.has(i)) return i;
  }
  return null;
}

/** 在指定屏位打开（或更换）影片：先回到多屏视图量出格位矩形，再交给后端。 */
async function openSlotFilm(slot, itemId, startMs) {
  if (multiBusy) return;
  multiBusy = true;
  try {
    browsingLibrary = false;
    applyView();
    const rect = rectOfSlot(slot);
    if (!rect) {
      showToast('格位不可用');
      return;
    }
    let view;
    try {
      view = await invoke('multi_open', {
        slot,
        itemId,
        x: rect.x,
        y: rect.y,
        width: rect.w,
        height: rect.h,
        startMs: startMs == null ? null : Math.round(startMs),
      });
    } catch (err) {
      showToast(String(err));
      return;
    }
    multiState = view;
    pausedSlots.delete(slot);
    draggingSlots.delete(slot);
    faultHidden.delete(slot);
    delete audioCache[slot];
    // 重开 / 换片：旧会话已被后端收尾，丢弃该格的字幕上下文
    dropSlotSubtitleContext(slot);
    renderMultiBar();
    renderMultiSlots();
    startTick();
  } finally {
    multiBusy = false;
  }
}

async function browseLibrary() {
  if (!inMulti) return;
  if (browsingLibrary) {
    browsingLibrary = false;
    applyView();
    renderMultiSlots();
    await placeAllSlots();
    return;
  }
  browsingLibrary = true;
  await invoke('multi_hide_all').catch(() => {});
  hideAllSubtitleLines();
  hidePreviewNote();
  applyView();
}

// ---- 单路 → 多屏 ----

/** 单路播放条「加入多屏」：收起单路播放，回到海报墙多选态并预勾选当前影片（0 号屏位）。 */
async function addToMulti() {
  if (!playingId) return;
  const id = playingId;
  await closePlayer();
  enterSelectMode(id);
  showToast('已把当前影片加入多屏：再选 1 部以上，点「进入多屏」');
}

// ---------------------------------------------------------------------------
// 主机模式（Console）：可切换的大屏 / 键盘导航界面
// ---------------------------------------------------------------------------
// 数据一律来自后端既有命令（不新增命令、不硬编码维度规则）：
// - 首页 Hero 与「最近观看」行：search_items(sort = last_watched) 的返回顺序
//   （排序在 L2 完成：最近观看过的最新一条在最前、从未播放的最后）；
// - 分类胶囊「未看 / 在看 / 已看」：后端既有派生维度 watch 的取值与频次（list_dimension_tags）；
// - 「所有影片」是清空筛选的动作；「文件缺失」是对全库结果按 missing 分区（数据可达性，非标签维度）。
// 组合语义（同维度 OR、跨维度 AND）仍由 L2 唯一实现，本层只提交「文本 + 维度筛选 + 排序」。

/** 后端既有派生维度 id：观看状态（未看 / 在看 / 已看）。取不到取值时整组胶囊隐藏，不猜。 */
const CONSOLE_STATUS_DIM = 'watch';

/** 主机模式的卡片尺寸档位（CSS 像素）：封面卡宽 / 宽幅卡宽 / 网格最小列宽。 */
const CONSOLE_CARD_PX = {
  large: { cover: 168, wide: 320, grid: 224 },
  mid: { cover: 148, wide: 284, grid: 196 },
  small: { cover: 116, wide: 240, grid: 168 },
};

const CONSOLE_COVER_MAX = 12; // 封面卡行最多展示条数
const CONSOLE_WIDE_MAX = 8;   // 宽幅卡行最多展示条数

let consoleOn = false;             // 主机模式是否激活（默认关闭，硬约束 C6）
let consoleScreen = 'home';        // 'home' | 'lib'
let consoleData = [];              // 全库 + 最近观看排序（Hero / 封面行 / 分类计数共用）
let consoleMissingOnly = false;    // 资源库屏「仅看文件缺失」（本地视图分区，非标签维度）
let consoleRows = [];              // 每次渲染重建：[{ kind, nodes: [HTMLElement] }]
let consoleFocus = { row: 0, col: 0, desiredCol: 0 };
let consoleDimsOpenBackup = false; // 进入主机模式前的维度面板开关，退出时还原
let consoleClockTimer = null;      // 顶栏时钟
let libraryStats = null;           // library_stats 缓存（经典与主机顶栏共用）

/** 主机层是否应当可见：播放中与多屏中一律隐藏，让位后端原生画面窗口。 */
function consoleShown() {
  return consoleOn && !inMulti && !playingId;
}

async function enterConsole() {
  if (consoleOn) {
    return;
  }
  if (inMulti) {
    showToast('多屏播放中不能进入主机模式，请先退出多屏');
    return;
  }
  consoleOn = true;
  try { localStorage.setItem('cinema.console', '1'); } catch (err) { /* 存储不可用时忽略 */ }
  // 互斥：多选态必须先退出；维度面板收起（记录原值，退出时还原）
  if (selectMode) {
    exitSelectMode();
  }
  consoleDimsOpenBackup = dimsOpen;
  dimsOpen = false;
  consoleMissingOnly = false;
  consoleScreen = 'home';
  els['c-settings-panel'].classList.add('hidden');
  els['c-preview-toggle'].checked = els['preview-toggle'].checked;
  applyView();
  startConsoleClock();
  await loadConsoleData();
  renderConsole();
}

function exitConsole() {
  if (!consoleOn) {
    return;
  }
  consoleOn = false;
  try { localStorage.setItem('cinema.console', '0'); } catch (err) { /* 忽略 */ }
  stopConsoleClock();
  dimsOpen = consoleDimsOpenBackup;
  consoleMissingOnly = false;
  els['c-settings-panel'].classList.add('hidden');
  els['c-search'].blur();
  clearConsoleFocus();
  applyView();
  refresh(); // 回到经典界面后同步一次库状态
}

function toggleConsole() {
  if (consoleOn) {
    exitConsole();
  } else {
    enterConsole();
  }
}

/** 播放 / 多屏状态变化后让主机层按当前状态回归（可见性由 applyView 决定，这里只补渲染）。 */
function syncConsole() {
  if (!consoleOn) {
    return;
  }
  applyView();
  if (consoleShown()) {
    loadConsoleData().then(renderConsole);
  }
}

/** 悬停预览开关：经典界面与主机模式设置浮层共用同一条命令与提示。 */
async function setPreviewEnabled(enabled) {
  els['preview-toggle'].checked = enabled;
  els['c-preview-toggle'].checked = enabled;
  try {
    await invoke('set_preview_enabled', { enabled });
  } catch (err) {
    showToast(String(err));
  }
  if (!enabled) {
    stopPreview();
  }
  showToast(enabled ? '已开启悬停预览' : '已关闭悬停预览');
}

/** 主机模式的全库数据：文本为空、无筛选、按最近观看排序。 */
async function loadConsoleData() {
  let list = [];
  try {
    list = await invoke('search_items', {
      query: { text: '', filters: {}, sort: 'last_watched' },
    });
  } catch (err) {
    list = [];
  }
  consoleData = Array.isArray(list) ? list : [];
  renderConsoleTopStats();
}

function renderConsoleTopStats() {
  const total = consoleData.length;
  if (libraryStats) {
    els['c-top-stats'].textContent =
      `条目 ${total} · 源 ${libraryStats.sources} · 解码器 ${libraryStats.decoderReady ? '就绪' : '不可用'}`;
  } else {
    els['c-top-stats'].textContent = `条目 ${total}`;
  }
}

/** Hero 元信息：用既有字段表达（本项目没有封面图与相对时间字段）。 */
function consoleItemMeta(item) {
  if (item.missing) {
    return '文件缺失，无法播放';
  }
  const parts = [];
  if (item.watched) {
    parts.push('已看完');
  } else if (item.positionMs > 0 && item.resumePercent != null) {
    parts.push(`已看 ${item.resumePercent}%`);
  }
  if (item.durationMs) {
    const remain = item.durationMs - item.positionMs;
    if (item.positionMs > 0 && !item.watched && remain > 0) {
      parts.push(`剩余 ${formatTime(remain)}`);
    } else {
      parts.push(`片长 ${formatTime(item.durationMs)}`);
    }
  }
  return parts.length ? parts.join(' · ') : '尚未观看';
}

/** Hero：宽幅底图 + 左下大标题 + 元信息行。底图与占位海报同一套色相公式，保持视觉一致。 */
function buildHero(item) {
  const hero = el('div', 'c-hero');
  hero.dataset.id = item.id;
  hero.title = item.title;

  const hue = hueOf(item.id);
  hero.style.background =
    `linear-gradient(158deg, hsl(${hue}, 40%, 27%), hsl(${(hue + 42) % 360}, 34%, 14%))`;
  hero.appendChild(el('div', 'ch-initial', (item.title || '影').trim().charAt(0).toUpperCase()));
  hero.appendChild(el('div', 'ch-grad'));
  hero.appendChild(el('div', 'ch-kicker', '最近观看'));

  const info = el('div', 'ch-info');
  const title = el('div', 'ch-title', item.title);
  if (item.missing) {
    title.appendChild(el('span', 'ch-flag missing', '文件缺失'));
  }
  info.appendChild(title);
  info.appendChild(el('div', 'ch-meta', consoleItemMeta(item)));
  hero.appendChild(info);

  hero.addEventListener('click', () => onCardClick(item));
  return hero;
}

/** 宽幅卡：复用 buildCard 的卡片结构（占位海报 / 标题 / 进度 / 角标），元信息放在卡片下方。 */
function buildWideItem(item) {
  const wrap = el('div', 'c-wide-item');
  const card = buildCard(item);
  wrap.appendChild(card);
  wrap.appendChild(el('div', 'c-wide-meta', consoleItemMeta(item)));
  return { wrap, card };
}

// ---- 分类胶囊（全部来自后端维度配置与本地分区） ----

/** 胶囊清单：所有影片 / 后端 watch 维度取值 / 文件缺失。 */
function consoleChips() {
  const out = [
    { key: 'all', label: '所有影片', count: consoleData.length, on: !hasQuery() && !consoleMissingOnly },
  ];
  const dim = dims.find((d) => d.id === CONSOLE_STATUS_DIM);
  for (const tag of (dim && dim.tags) || []) {
    out.push({
      key: 'status',
      label: tag.name,
      count: tag.count,
      on: (selected[CONSOLE_STATUS_DIM] || []).includes(tag.name),
    });
  }
  const missing = consoleData.filter((i) => i.missing).length;
  if (missing > 0) {
    out.push({ key: 'missing', label: '文件缺失', count: missing, on: consoleMissingOnly });
  }
  return out;
}

function renderConsoleChips(host, isHome) {
  const nodes = [];
  for (const spec of consoleChips()) {
    const chip = el('button', 'chip' + (spec.on ? ' on' : ''));
    chip.type = 'button';
    chip.appendChild(document.createTextNode(spec.label));
    if (spec.count > 0) {
      chip.appendChild(el('span', 'cnt', String(spec.count)));
    }
    chip.title = spec.key === 'all' ? '清空筛选，显示全部影片' : `${spec.label}（${spec.count}）`;
    chip.addEventListener('click', () => onConsoleChipClick(spec.key, spec.label, isHome));
    host.appendChild(chip);
    nodes.push(chip);
  }
  return nodes;
}

function onConsoleChipClick(key, value, isHome) {
  // 首页的分类胶囊是「入口」：点击后切到资源库屏再看结果
  if (isHome) {
    consoleScreen = 'lib';
    consoleFocus = { row: 0, col: 0, desiredCol: 0 };
  }
  if (key === 'all') {
    consoleMissingOnly = false;
    selected = {};
    renderFilterBar();
    renderDims();
    refresh();
    return;
  }
  if (key === 'missing') {
    consoleMissingOnly = !consoleMissingOnly;
    renderConsole();
    return;
  }
  consoleMissingOnly = false;
  toggleTag(CONSOLE_STATUS_DIM, value); // 内部会 refresh()，随之 renderConsole()
}

// ---- 渲染 ----

/** 资源库屏当前要展示的条目：当前查询结果，必要时再按「文件缺失」做本地分区。 */
function consoleVisibleItems() {
  return consoleMissingOnly ? items.filter((i) => i.missing) : items.slice();
}

function renderConsole() {
  const home = consoleScreen === 'home';
  els['c-screen-home'].classList.toggle('hidden', !home);
  els['c-screen-lib'].classList.toggle('hidden', home);
  if (home) {
    renderConsoleHome();
  } else {
    renderConsoleLibrary();
  }

  const visible = home ? consoleData : consoleVisibleItems();
  const empty = visible.length === 0;
  els['c-empty'].classList.toggle('hidden', !empty);
  if (empty) {
    els['c-screen-home'].classList.add('hidden');
    els['c-screen-lib'].classList.add('hidden');
    consoleRows = [];
    consoleFocus = { row: 0, col: 0, desiredCol: 0 };
    renderConsoleEmpty(home);
  } else {
    els['c-empty'].replaceChildren();
  }
  applyConsoleFocus();
}

function renderConsoleHome() {
  consoleRows = [];
  els['c-hero'].replaceChildren();
  els['c-cover-row'].replaceChildren();
  els['c-chips'].replaceChildren();
  els['c-wide-row'].replaceChildren();

  // Hero：全库「最近观看」的第一条（后端按最近观看排序）
  const hero = consoleData[0] || null;
  els['c-hero'].classList.toggle('hidden', !hero);
  if (hero) {
    const node = buildHero(hero);
    els['c-hero'].appendChild(node);
    consoleRows.push({ kind: 'hero', nodes: [node] });
  }

  // 最近观看封面卡行
  const coverItems = consoleData.slice(0, CONSOLE_COVER_MAX);
  for (const item of coverItems) {
    els['c-cover-row'].appendChild(buildCard(item));
  }
  if (coverItems.length) {
    consoleRows.push({ kind: 'cover', nodes: Array.from(els['c-cover-row'].children) });
  }

  // 分类胶囊行
  const chips = renderConsoleChips(els['c-chips'], true);
  if (chips.length) {
    consoleRows.push({ kind: 'chips', nodes: chips });
  }

  // 继续观看宽幅卡行：有进度、未看完、未缺失，按最近观看排序取前若干
  const wideNodes = [];
  const wideItems = consoleData
    .filter((i) => !i.missing && i.positionMs > 0 && !i.watched)
    .slice(0, CONSOLE_WIDE_MAX);
  for (const item of wideItems) {
    const built = buildWideItem(item);
    els['c-wide-row'].appendChild(built.wrap);
    wideNodes.push(built.card);
  }
  if (wideNodes.length) {
    consoleRows.push({ kind: 'wide', nodes: wideNodes });
  }
}

function renderConsoleLibrary() {
  consoleRows = [];
  els['c-chips-lib'].replaceChildren();
  els['c-grid'].replaceChildren();

  const chips = renderConsoleChips(els['c-chips-lib'], false);
  if (chips.length) {
    consoleRows.push({ kind: 'chips', nodes: chips });
  }

  const list = consoleVisibleItems();
  for (const item of list) {
    els['c-grid'].appendChild(buildCard(item));
  }
  if (list.length) {
    consoleRows.push({ kind: 'grid', nodes: Array.from(els['c-grid'].children) });
  }
}

/** 两种空态分开：空库引导去经典界面入库；筛选无结果列出生效标签 + 清空（不自动放宽条件）。 */
function renderConsoleEmpty(home) {
  const box = els['c-empty'];
  box.replaceChildren();

  if (home && !consoleData.length) {
    box.appendChild(el('div', 'empty-title', '还没有内容'));
    box.appendChild(
      el('div', 'empty-sub', '主机模式只提供浏览与播放；请回到经典界面填入本地目录并「扫描入库」。'),
    );
    const actions = el('div', 'empty-actions');
    const back = el('button', 'btn primary', '回到经典界面');
    back.type = 'button';
    back.addEventListener('click', exitConsole);
    actions.appendChild(back);
    box.appendChild(actions);
    return;
  }

  box.appendChild(el('div', 'empty-title', '没有符合条件的影片'));
  box.appendChild(
    el('div', 'empty-sub', '同维度内多选为「或」，跨维度之间为「与」；此处不会自动放宽条件。'),
  );

  const entries = activeEntries();
  if (entries.length) {
    const chips = el('div', 'empty-chips');
    for (const entry of entries) {
      chips.appendChild(removeChip(entry));
    }
    box.appendChild(chips);
  }
  if (consoleMissingOnly) {
    box.appendChild(el('div', 'empty-sub', '当前视图：仅看文件缺失'));
  }

  const actions = el('div', 'empty-actions');
  const clear = el('button', 'btn primary', '清空筛选');
  clear.type = 'button';
  clear.addEventListener('click', () => {
    consoleMissingOnly = false;
    selected = {};
    renderFilterBar();
    renderDims();
    refresh();
  });
  actions.appendChild(clear);
  box.appendChild(actions);
}

// ---- 焦点模型与键盘导航 ----

/** 资源库网格当前列数（用于 ↑↓ 跨行移动）；布局未完成时退化为 1。 */
function consoleGridCols() {
  const tpl = getComputedStyle(els['c-grid']).gridTemplateColumns;
  if (!tpl) {
    return 1;
  }
  const n = tpl.split(' ').filter(Boolean).length;
  return n > 0 ? n : 1;
}

function clearConsoleFocus() {
  for (const band of consoleRows) {
    for (const node of band.nodes) {
      node.classList.remove('focused');
    }
  }
}

/** 把焦点节点滚入可视区：横向行做居中，纵向只滚最小距离（禁用 scrollIntoView，避免整屏跳动）。 */
function scrollConsoleTo(node) {
  const rowEl = node.closest('.c-row');
  if (rowEl) {
    const nr = node.getBoundingClientRect();
    const rr = rowEl.getBoundingClientRect();
    const delta = nr.left - rr.left - (rowEl.clientWidth - nr.width) / 2;
    if (Math.abs(delta) > 1) {
      rowEl.scrollLeft += delta;
    }
  }
  const screen = node.closest('.c-screen');
  if (screen) {
    const nr = node.getBoundingClientRect();
    const sr = screen.getBoundingClientRect();
    if (nr.top < sr.top) {
      screen.scrollTop -= sr.top - nr.top;
    } else if (nr.bottom > sr.bottom) {
      screen.scrollTop += nr.bottom - sr.bottom;
    }
  }
}

/** 统一出口：清旧焦点 → 夹紧行列 → 给当前节点加焦点 → 滚入可视区。渲染末尾必须调用。 */
function applyConsoleFocus() {
  clearConsoleFocus();
  if (!consoleRows.length) {
    return;
  }
  // 行集可能因切换屏（首页 4 行 ↔ 资源库 2 行）而变短，夹紧避免焦点落在不存在的行上
  if (consoleFocus.row >= consoleRows.length) {
    consoleFocus.row = consoleRows.length - 1;
  }
  if (consoleFocus.row < 0) {
    consoleFocus.row = 0;
  }
  const band = consoleRows[consoleFocus.row];
  if (!band || !band.nodes.length) {
    return;
  }
  const slot = Math.max(0, Math.min(consoleFocus.col, band.nodes.length - 1));
  consoleFocus.col = slot;
  const node = band.nodes[slot];
  if (!node) {
    return;
  }
  node.classList.add('focused');
  scrollConsoleTo(node);
}

function moveConsoleFocus(dRow, dCol) {
  if (!consoleRows.length) {
    return;
  }
  const band = consoleRows[consoleFocus.row];
  if (!band || !band.nodes.length) {
    return;
  }
  const perRow = band.kind === 'grid' ? consoleGridCols() : band.nodes.length;

  if (dCol !== 0) {
    // 行内首尾夹紧，不环绕（Steam 行为）
    consoleFocus.col = Math.max(0, Math.min(consoleFocus.col + dCol, band.nodes.length - 1));
    consoleFocus.desiredCol = consoleFocus.col;
  }

  if (dRow !== 0) {
    const next = consoleFocus.col + dRow * perRow;
    if (next >= 0 && next < band.nodes.length) {
      consoleFocus.col = next; // 同一带内跨行（仅网格会发生）
    } else {
      const r = consoleFocus.row + (dRow > 0 ? 1 : -1);
      if (r < 0 || r >= consoleRows.length) {
        return;
      }
      consoleFocus.row = r;
      const target = consoleRows[r];
      consoleFocus.col = Math.max(0, Math.min(consoleFocus.desiredCol, target.nodes.length - 1));
    }
    consoleFocus.desiredCol = consoleFocus.col;
  }
  applyConsoleFocus();
}

/** Enter / 空格 = 焦点项主操作，与鼠标点击走同一条路径。 */
function activateConsoleFocus() {
  const band = consoleRows[consoleFocus.row];
  if (!band) {
    return;
  }
  const node = band.nodes[consoleFocus.col];
  if (node) {
    node.click();
  }
}

/** Esc：设置浮层 → 资源库屏返回首页 → 首页退出主机模式。 */
function consoleEscape() {
  if (consoleScreen === 'lib') {
    consoleScreen = 'home';
    consoleFocus = { row: 0, col: 0, desiredCol: 0 };
    renderConsole();
    return;
  }
  exitConsole();
}

/** 是否有弹层占据交互（模态或字幕菜单）。主机导航在此期间整体挂起，Esc 交给弹层。 */
function modalOpen() {
  if (document.querySelector('.modal:not(.hidden)')) {
    return true;
  }
  const menu = els['subtitle-menu'];
  return !!menu && !menu.classList.contains('hidden');
}

/** 输入态（搜索框 / 下拉）下不消费导航键，避免影响文本编辑。 */
function isTypingTarget(node) {
  const t = node || document.activeElement;
  if (!t || !t.tagName) {
    return false;
  }
  const tag = t.tagName.toUpperCase();
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || !!t.isContentEditable;
}

function onConsoleKey(event) {
  // F10 全局切换（经典界面下也生效）
  if (event.key === 'F10') {
    event.preventDefault();
    toggleConsole();
    return;
  }
  if (!consoleOn) {
    return;
  }
  if (modalOpen()) {
    return; // 弹层存在时把键盘交给弹层，不改现有二选一语义
  }
  if (!consoleShown()) {
    return; // 播放中主机层已隐藏，键盘交还给经典界面
  }
  if (event.key === 'Escape' && !els['c-settings-panel'].classList.contains('hidden')) {
    event.preventDefault();
    els['c-settings-panel'].classList.add('hidden');
    return;
  }
  if (isTypingTarget(event.target)) {
    if (event.key === 'Escape' && els['c-search'].value) {
      event.preventDefault();
      els['c-search'].value = '';
      searchText = '';
      refresh();
    }
    return;
  }

  switch (event.key) {
    case 'ArrowLeft':
      event.preventDefault();
      moveConsoleFocus(0, -1);
      break;
    case 'ArrowRight':
      event.preventDefault();
      moveConsoleFocus(0, 1);
      break;
    case 'ArrowUp':
      event.preventDefault();
      moveConsoleFocus(-1, 0);
      break;
    case 'ArrowDown':
      event.preventDefault();
      moveConsoleFocus(1, 0);
      break;
    case 'Enter':
    case ' ':
      event.preventDefault();
      activateConsoleFocus();
      break;
    case 'Escape':
      event.preventDefault();
      consoleEscape();
      break;
    default:
      break;
  }
}

// ---- 顶栏时钟与设置浮层 ----

function tickConsoleClock() {
  const now = new Date();
  const hh = String(now.getHours()).padStart(2, '0');
  const mm = String(now.getMinutes()).padStart(2, '0');
  els['c-clock'].textContent = `${hh}:${mm}`;
}

function startConsoleClock() {
  tickConsoleClock();
  stopConsoleClock();
  consoleClockTimer = setInterval(tickConsoleClock, 20000);
}

function stopConsoleClock() {
  clearInterval(consoleClockTimer);
  consoleClockTimer = null;
}

/** 进入资源库屏并聚焦搜索框（顶栏「搜索」入口）。 */
function openConsoleSearch() {
  consoleScreen = 'lib';
  consoleFocus = { row: 0, col: 0, desiredCol: 0 };
  renderConsole();
  els['c-search'].focus();
  els['c-search'].select();
}

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

async function bindNativeEvents() {
  // 画面窗口截获的点击：预览 → 进入播放页；正式播放 → 切换暂停
  await listen('preview-click', () => {
    const id = previewItemId;
    const item = items.find((i) => i.id === id);
    stopPreview();
    if (item) {
      openItem(item);
    }
  });
  await listen('player-click', () => {
    togglePause();
  });
  // 多屏每格画面截获的点击：把该格切为发声窗（PRD FR-06 点击某格发声）
  await listen('multi-click', (event) => {
    const slot = Number(event && event.payload);
    if (Number.isFinite(slot) && inMulti) {
      setSpeaker(slot);
    }
  });
}

async function init() {
  try {
    constants = await invoke('ui_constants');
  } catch (err) {
    /* 常量取不到时沿用默认值 */
  }
  // 控制条按后端物理像素预留，这里换算成 CSS 像素以精确对齐画面窗口下沿
  document.documentElement.style.setProperty('--bar-h', `${constants.controlBarPx / DPR}px`);

  els['dir-browse'].addEventListener('click', browseDir);
  els['add-btn'].addEventListener('click', addSource);
  els['scan-btn'].addEventListener('click', scanAll);
  els['scan-cancel'].addEventListener('click', cancelScan);
  els['dir-input'].addEventListener('keydown', (event) => {
    if (event.key === 'Enter') {
      addSource();
    }
  });
  // 数据迁移（FR-07）：导出为包 / 从包导入
  els['data-btn'].addEventListener('click', openDataModal);
  els['data-export'].addEventListener('click', exportLibrary);
  els['data-import'].addEventListener('click', importLibrary);
  els['data-cancel'].addEventListener('click', hideDataModal);
  els['data-path'].addEventListener('keydown', (event) => {
    if (event.key === 'Enter') {
      exportLibrary();
    }
  });
  // 联网导入（M7）：粘贴列表页 URL，逐条交给 yt-dlp
  els['online-btn'].addEventListener('click', openOnlineModal);
  els['online-import'].addEventListener('click', onlineImport);
  els['online-cancel'].addEventListener('click', hideOnlineModal);
  // 悬停预览开关：经典顶栏与主机模式设置浮层共用同一条命令与提示
  els['preview-toggle'].addEventListener('change', () => {
    setPreviewEnabled(els['preview-toggle'].checked);
  });

  // 检索区：搜索框输入防抖后即时生效；排序与海报尺寸切换同样即时生效
  els['search-input'].addEventListener('input', () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      searchText = els['search-input'].value;
      refresh();
    }, 200);
  });
  els['sort-select'].addEventListener('change', () => {
    sortTag = els['sort-select'].value;
    refresh();
  });
  els['filter-toggle'].addEventListener('click', () => {
    dimsOpen = !dimsOpen;
    applyView();
  });
  els['size-group'].addEventListener('click', (event) => {
    const btn = event.target.closest('.seg-btn');
    if (btn) {
      applyPosterSize(btn.dataset.size);
    }
  });

  els['pb-toggle'].addEventListener('click', togglePause);
  els['pb-close'].addEventListener('click', closePlayer);
  els['pb-seek'].addEventListener('input', () => { seeking = true; });
  els['pb-seek'].addEventListener('change', async () => {
    seeking = false;
    if (!currentDurationMs) {
      return;
    }
    const positionMs = Math.round((Number(els['pb-seek'].value) / 1000) * currentDurationMs);
    try {
      await invoke('seek', { positionMs });
    } catch (err) {
      showToast(String(err));
    }
    // seek 丢弃旧句：清空本地累积的句子与复用轨道当前句（后端也会 on_seek）
    resetSubtitleText(null);
    renderSubtitleLines(null);
  });

  els['resume-continue'].addEventListener('click', () => {
    const item = resumeItem;
    const offer = resumeOffer;
    hideResume();
    if (item && offer) {
      play(item.id, offer.positionMs, item);
    }
  });
  els['resume-restart'].addEventListener('click', () => {
    const item = resumeItem;
    hideResume();
    if (item) {
      play(item.id, null, item);
    }
  });
  els['resume-cancel'].addEventListener('click', hideResume);

  // 多屏播放（PRD FR-06）：进入/退出多选态、进入/退出多屏、布局与上限、退出二选一
  els['multi-toggle'].addEventListener('click', toggleSelectMode);
  els['select-enter'].addEventListener('click', enterMulti);
  els['select-cancel'].addEventListener('click', exitSelectMode);
  els['browse-btn'].addEventListener('click', browseLibrary);
  els['exit-multi'].addEventListener('click', openMultiExit);
  els['exit-keep'].addEventListener('click', () => exitMulti('keep_progress'));
  els['exit-stop'].addEventListener('click', () => exitMulti('stop_all'));
  els['exit-cancel'].addEventListener('click', hideMultiExit);
  els['limit-select'].addEventListener('change', () => setLimit(els['limit-select'].value));
  els['concurrent-audio'].addEventListener('change', () => setConcurrentAudio(els['concurrent-audio'].checked));
  els['pb-add-multi'].addEventListener('click', addToMulti);
  await loadLayouts();

  // 主机模式（Console）：入口按钮 / 顶栏入口 / 设置浮层 / 尺寸组 / 资源库搜索框
  els['console-btn'].addEventListener('click', () => toggleConsole());
  els['c-search-btn'].addEventListener('click', openConsoleSearch);
  els['c-to-classic'].addEventListener('click', exitConsole);
  els['c-to-classic-2'].addEventListener('click', exitConsole);
  els['c-settings'].addEventListener('click', () => {
    els['c-settings-panel'].classList.toggle('hidden');
  });
  els['c-settings-close'].addEventListener('click', () => {
    els['c-settings-panel'].classList.add('hidden');
  });
  els['c-preview-toggle'].addEventListener('change', () => {
    setPreviewEnabled(els['c-preview-toggle'].checked);
  });
  els['c-size-group'].addEventListener('click', (event) => {
    const btn = event.target.closest('.seg-btn');
    if (btn) {
      applyPosterSize(btn.dataset.size);
    }
  });
  // 资源库搜索框与经典搜索框共用同一条检索路径（文本 + 维度筛选 + 排序）
  els['c-search'].addEventListener('input', () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      searchText = els['c-search'].value;
      els['search-input'].value = searchText;
      refresh();
    }, 200);
  });

  bindSubtitleEvents();

  // 主机模式键盘导航：capture 阶段拦截方向键 / Enter / Esc / F10（F10 在经典界面下亦生效）
  window.addEventListener('keydown', onConsoleKey, { capture: true });

  // 预览正在播放时，一旦指针离开其所占矩形就停止
  window.addEventListener('mousemove', (event) => {
    if (previewRect && pointerOutsidePreview(event)) {
      stopPreview();
    }
  });
  window.addEventListener('blur', stopPreview);
  window.addEventListener('resize', () => {
    stopPreview();
    // 窗口尺寸变化后格位矩形随之改变，重新摆放原生画面窗口
    if (inMulti && !browsingLibrary) placeAllSlots();
    // 主机模式：卡片尺寸随窗口变化，焦点需要重新滚入可视区
    if (consoleOn) applyConsoleFocus();
  });
  els.wall.addEventListener('scroll', stopPreview);

  await bindNativeEvents();
  applyPosterSize('mid');
  await reloadFilters();
  await refresh();
  // 记忆用户上次的主机模式选择（默认关闭，硬约束 C6）
  try {
    if (localStorage.getItem('cinema.console') === '1') {
      await enterConsole();
    }
  } catch (err) {
    /* 存储不可用时保持经典界面 */
  }
}

if (!invoke) {
  document.getElementById('empty').innerHTML =
    '<div class="empty-title">前端未连接到桌面端</div>' +
    '<div class="empty-sub">请通过 cinema-desktop 启动，而不是直接打开本页面。</div>';
} else {
  init();
}
