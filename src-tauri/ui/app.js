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
  'add-btn', 'data-btn', 'online-btn', 'manage-btn', 'meta-btn', 'preview-toggle', 'stats',
  'manage-modal', 'manage-list', 'manage-close', 'manage-close-2',
  'manage-summary', 'manage-source-count', 'manage-detail',
  'meta-modal', 'meta-base', 'meta-cookie', 'meta-status', 'meta-save', 'meta-scrape-all', 'meta-close',
  'llm-base', 'llm-key', 'llm-model', 'llm-status',
  'llm-provider', 'llm-provider-model', 'llm-custom', 'llm-model-preset',
  'meta-progress', 'meta-progress-text', 'meta-progress-pct', 'meta-progress-fill', 'meta-progress-current',
  'detail-modal', 'detail-poster', 'detail-title', 'detail-meta', 'detail-tags',
  'detail-play', 'detail-trailer', 'detail-refresh', 'detail-x', 'detail-state',
  'detail-shots', 'detail-comments', 'trailer-stage', 'trailer-frame', 'lightbox', 'lightbox-img',
  'trailer-controls', 'trailer-seek', 'trailer-time', 'trailer-close', 'player-backdrop',
  'data-modal', 'data-path', 'data-export', 'data-import', 'data-cancel',
  'scan-overlay', 'scan-title', 'scan-bar-fill', 'scan-detail', 'scan-current', 'scan-cancel',
  'online-modal', 'online-url', 'online-import', 'online-cancel', 'online-result',
  'searchbar', 'search-input', 'filter-toggle', 'tag-back', 'multi-toggle', 'sort-select', 'size-group', 'filter-bar', 'dims',
  'wall', 'empty', 'grid', 'sources',
  'select-bar', 'select-count', 'select-all', 'select-enter', 'select-remove', 'select-cancel',
  'multi-bar', 'layout-bar', 'multi-count', 'limit-select', 'concurrent-audio', 'browse-btn', 'exit-multi', 'multi-stage',
  'multi-exit-modal', 'exit-keep', 'exit-stop', 'exit-cancel',
  'resume-modal', 'resume-title', 'resume-desc', 'resume-continue', 'resume-restart', 'resume-cancel',
  'player-bar', 'pb-title', 'pb-toggle', 'pb-seek', 'pb-time', 'pb-close',
  'pb-subtitle', 'pb-sublive', 'pb-add-multi', 'pb-settings',
  'player-settings', 'pset-close', 'pset-forward', 'pset-preview-delay', 'pset-preview-sound', 'pset-save',
  'preview-stage', 'preview-frame', 'preview-controls', 'preview-title', 'preview-sound', 'preview-close',
  'subtitle-overlay', 'sub-origin', 'sub-translation',
  'subtitle-menu', 'sm-close', 'sm-state', 'sm-tracks', 'sm-notice',
  'sm-online-query', 'sm-online-search', 'sm-online-status', 'sm-online-list',
  'subtitle-privacy', 'sub-privacy-note', 'sub-engine-list', 'sub-privacy-confirm', 'sub-privacy-cancel',
  'subtitle-failure', 'sub-failure-desc', 'sub-fail-external', 'sub-fail-dictation', 'sub-fail-close',
  'subtitle-external', 'sub-external-path', 'sub-external-confirm', 'sub-external-cancel',
  'preview-note', 'toast',
  // 主机模式（Console）
  'console', 'console-btn', 'c-top-stats', 'c-clock', 'c-search-btn', 'c-search', 'c-settings', 'c-to-classic',
  'c-screen-home', 'c-screen-lib', 'c-cover-row', 'c-chips', 'c-wide-row',
  'c-rec-row', 'c-rec-status', 'c-rec-refresh',
  'c-chips-lib', 'c-grid', 'c-empty', 'c-amb-a', 'c-amb-b', 'c-focus-title',
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
/** 最近一次已知播放位置（毫秒），快进以此为基准。 */
let lastPositionMs = 0;
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
const CARD_MIN_PX = { large: 640, mid: 520, small: 420 };

// 悬停预览状态
let hoverTimer = null;
let previewItemId = null;
let previewStopTimer = null;
let previewRect = null; // 物理像素 { x, y, w, h }
let previewToken = 0;

// 详情页预告片进度
let trailerTickTimer = null;
let trailerDurationMs = 0;
let trailerSeeking = false;

// 单路播放时画面底部额外留出的空间（CSS 像素），供字幕菜单 / 弹层显示
let playerReserveCss = 0;

// 控制条自动隐藏：播放中 3 秒无鼠标移动即收起，画面随之铺到窗口底部
const PLAYER_BAR_IDLE_MS = 3000;
let playerBarHidden = false;
let playerBarHideTimer = null;
/**
 * 收起控制条会改变原生画面窗口的几何：窗口在指针下方被撑开时，系统会补发
 * 「位置没变」的移动事件（页面层也会收到等价事件）。这个时间戳用于丢弃紧跟在
 * 收起动作之后的这些假事件，否则控制条刚收起就被叫回来，画面来回闪。
 */
let playerBarWakeBlockUntil = 0;
/** 已下发给后端的底部留出量（物理像素），避免重复调用。 */
let playerSentReserve = 0;

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

/** 维度面板里的取值标签：点击即筛选，再点一次取消（PRD FR-03「单标签点击」）。
 *  `tag` 带 id 时右侧额外给一个全库删除入口（只删标签，不动视频文件）。 */
function tagChip(dim, name, count, tag) {
  const on = (selected[dim.id] || []).includes(name);
  const chip = el('button', 'chip' + (on ? ' on' : ''));
  chip.type = 'button';
  chip.appendChild(document.createTextNode(name));
  if (typeof count === 'number' && count > 0) {
    chip.appendChild(el('span', 'cnt', String(count)));
  }
  if (tag && tag.id) {
    const del = el('span', 'chip-del', '×');
    del.title = `从全库删除标签「${name}」`;
    del.addEventListener('click', (event) => {
      // 删除是独立动作：不能顺带触发这个标签的筛选切换
      event.stopPropagation();
      deleteTagValue(dim, tag);
    });
    chip.appendChild(del);
  }
  chip.title = `${dim.label} · ${name}`;
  chip.addEventListener('click', () => toggleTag(dim.id, name));
  return chip;
}

/** 全库删除一个标签取值：所有影片上的该标签一并移除。 */
async function deleteTagValue(dim, tag) {
  const ok = await confirmDialog(
    '从全库删除标签？',
    `将把「${dim.label} · ${tag.name}」从 ${tag.count || 0} 部影片上移除，并从标签清单里删除该取值。\n` +
      '只删库内标签记录，不会删除任何视频文件。确定继续吗？',
  );
  if (!ok) {
    return;
  }
  try {
    const count = await invoke('delete_dimension_tag', { tagId: tag.id });
    if (selected[dim.id]) {
      selected[dim.id] = selected[dim.id].filter((value) => value !== tag.name);
      if (!selected[dim.id].length) {
        delete selected[dim.id];
      }
    }
    renderFilterBar();
    await reloadFilters();
    await refresh();
    showToast(`已删除标签「${tag.name}」，影响 ${count} 部影片`);
  } catch (err) {
    showToast(String(err));
  }
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
      body.appendChild(tagChip(dim, tag.name, tag.count, tag));
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
      el('div', 'empty-sub', '在上方点「添加目录」选择文件夹，确认后会自动扫描该目录入库。'),
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
/** 封面加载后保持卡片横版铺满，不再按图片比例改写卡片尺寸。 */
function fitCardToPoster(img) {
  // 横版卡固定 16:9，由 CSS 决定；这里保留空实现只用于兼容旧调用点。
  void img;
}

function loadCover(itemId, img) {
  (async () => {
    const cached = coverCache.get(itemId);
    if (cached) {
      img.src = cached;
      fitCardToPoster(img);
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
          fitCardToPoster(img);
          // 主机模式环境底正展示该条目：封面刚到就立刻替换渐变兜底
          if (consoleOn && consoleAmbientId === itemId) {
            paintConsoleAmbient(consoleItemById(itemId), true);
          }
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
  card.title = `${item.title}（单击查看详情：封面 / 截图 / 标签 / 评论 / 预告片）`;

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
    if (!consoleOn) {
      applyClassicFocus(card);
    }
    scheduleHover(item, card);
  });
  card.addEventListener('mouseleave', () => {
    if (!consoleOn) {
      clearClassicFocus(card);
    }
    cancelHover();
    if (consoleOn) {
      cancelConsolePreview();
    }
  });
  card.addEventListener('click', () => onCardClick(item));

  return card;
}

/** 经典模式：鼠标所在卡放大，左右各两张后退压暗（与主机模式同一套观感）。 */
function applyClassicFocus(card) {
  const parent = card.parentElement;
  if (!parent) {
    return;
  }
  const peers = Array.from(parent.children).filter((n) => n.classList.contains('card'));
  const at = peers.indexOf(card);
  peers.forEach((peer, i) => {
    peer.classList.toggle('focused', peer === card);
    peer.classList.toggle('near', peer !== card && Math.abs(i - at) <= 2);
  });
}

function clearClassicFocus(card) {
  // 焦点已被别的卡接管时不要误清（快速划过相邻卡会先 leave 后 enter）
  if (!card.classList.contains('focused')) {
    return;
  }
  const parent = card.parentElement;
  if (!parent) {
    return;
  }
  for (const peer of parent.children) {
    if (peer.classList && peer.classList.contains('card')) {
      peer.classList.remove('focused');
      peer.classList.remove('near');
    }
  }
}

/** 单张海报的选中态与序号（多选态下可见）。 */
function markSelected(card, item) {
  const at = selectedIds.indexOf(item.id);
  card.classList.toggle('selected', at >= 0);
  const pick = card.querySelector('.pick');
  if (pick) pick.textContent = at >= 0 ? String(at + 1) : '';
}

/** 海报点击分三种语义：多选态挑片、多屏浏览库时加入空位、否则打开详情页。 */
function onCardClick(item) {
  if (selectMode) {
    toggleSelect(item);
    return;
  }
  if (inMulti) {
    addCardToMulti(item);
    return;
  }
  openDetail(item);
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

/** 预览舞台（CSS 矩形）：居中放大，面积 ≥ 1/4 全屏（0.62 × 0.62 ≈ 38%）。 */
function previewStageRect() {
  const w = Math.round(window.innerWidth * 0.62);
  const h = Math.round(window.innerHeight * 0.62);
  return {
    left: Math.round((window.innerWidth - w) / 2),
    top: Math.round((window.innerHeight - h) / 2),
    width: w,
    height: h,
  };
}

/** 海报/焦点卡的位置（CSS），用于「预览不可用」提示与指针离开判定。 */
let previewAnchorRect = null;

function scheduleHover(item, card) {
  cancelHover();
  if (consoleOn) {
    // 主机模式同样允许鼠标停留预览：先把焦点落到该卡，再走焦点停留计时
    focusConsoleNode(card);
    return;
  }
  if (playingId) {
    return;
  }
  hoverTimer = setTimeout(() => startPreview(item, card), previewDelayMs());
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
  // 画面固定在屏幕中央放大展示，不再贴合海报卡片
  const stage = previewStageRect();
  const x = toPhysical(stage.left);
  const y = toPhysical(stage.top);
  const width = toPhysical(stage.width);
  const height = toPhysical(stage.height);
  const cardRect = card ? card.getBoundingClientRect() : null;

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
    previewAnchorRect = null;
    // 拒绝不是静默失败：在海报浮层标注原因（PRD FR-02 / §11）
    if (result.reason && !String(result.reason).includes('关闭')) {
      showPreviewNote(card, result.reason);
    }
    return;
  }

  previewItemId = item.id;
  previewRect = { x, y, w: width, h: height };
  previewAnchorRect = cardRect;
  renderPreviewStage(item, stage);
  // 预览偏好：声音开关（默认静音）；随机片段避开片头
  invoke('set_preview_sound', { enabled: previewSoundOn }).catch(() => {});
  startPreviewRandomSegment(token);
  clearTimeout(previewStopTimer);
  previewStopTimer = setTimeout(stopPreview, constants.previewSegmentMs);
}

/** 展开预览舞台的 HTML 层：四周暗底、描边框与画面下方控制条。 */
function renderPreviewStage(item, stage) {
  els['preview-stage'].classList.remove('hidden');
  els['preview-title'].textContent = item.title || '—';
  const frame = els['preview-frame'];
  frame.style.left = `${stage.left - 4}px`;
  frame.style.top = `${stage.top - 4}px`;
  frame.style.width = `${stage.width + 8}px`;
  frame.style.height = `${stage.height + 8}px`;
  const controls = els['preview-controls'];
  controls.style.left = `${stage.left}px`;
  controls.style.top = `${stage.top + stage.height + 14}px`;
  controls.style.width = `${stage.width}px`;
  updatePreviewSoundUi();
}

function updatePreviewSoundUi() {
  els['preview-sound'].textContent = previewSoundOn ? '声音：开' : '声音：关';
  els['preview-sound'].classList.toggle('primary', previewSoundOn);
}

/** 预览声音开关：即时下发到预览实例，并记住本机偏好。 */
function togglePreviewSound() {
  previewSoundOn = !previewSoundOn;
  try {
    localStorage.setItem(PREVIEW_SOUND_KEY, previewSoundOn ? '1' : '0');
  } catch (err) {
    /* 存储不可用时仅本次生效 */
  }
  invoke('set_preview_sound', { enabled: previewSoundOn }).catch(() => {});
  updatePreviewSoundUi();
}

/**
 * 随机片段：预览实例装载完成后读到真实时长，再跳到一个随机位置。
 * 本地预告片与整片共用同一实例，这里不假设素材类型。
 */
let previewRandomTimer = null;

function startPreviewRandomSegment(token) {
  clearTimeout(previewRandomTimer);
  let tries = 0;
  const probe = async () => {
    if (token !== previewToken || !previewItemId) return;
    try {
      const status = await invoke('trailer_status');
      if (status && status.active && status.durationMs > 8000) {
        const span = Math.max(0, status.durationMs - 6000);
        const target = Math.floor(span * (0.1 + Math.random() * 0.7));
        await invoke('trailer_seek', { positionMs: target });
        return;
      }
    } catch (err) {
      /* 属性暂不可读：继续重试 */
    }
    tries += 1;
    if (tries < 20) previewRandomTimer = setTimeout(probe, 250);
  };
  previewRandomTimer = setTimeout(probe, 300);
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
  cancelConsolePreview();
  clearTimeout(previewStopTimer);
  previewStopTimer = null;
  clearTimeout(previewRandomTimer);
  previewRandomTimer = null;
  stopTrailerTick();
  els['trailer-stage'].classList.add('hidden');
  els['preview-stage'].classList.add('hidden');
  previewToken += 1;
  const wasActive = previewItemId !== null;
  previewItemId = null;
  previewRect = null;
  previewAnchorRect = null;
  if (wasActive) {
    invoke('stop_preview').catch(() => {});
  }
}

/** 预告片心跳：读取预览实例进度，推进画面下方的进度条。 */
function startTrailerTick() {
  stopTrailerTick();
  trailerDurationMs = 0;
  trailerSeeking = false;
  els['trailer-seek'].value = 0;
  els['trailer-time'].textContent = '00:00 / --:--';
  trailerTickTimer = setInterval(pollTrailer, 400);
}

function stopTrailerTick() {
  if (trailerTickTimer) {
    clearInterval(trailerTickTimer);
    trailerTickTimer = null;
  }
}

async function pollTrailer() {
  if (!previewItemId) {
    return;
  }
  let status;
  try {
    status = await invoke('trailer_status');
  } catch (err) {
    return;
  }
  if (!status || !status.active) {
    stopPreview();
    return;
  }
  trailerDurationMs = status.durationMs || trailerDurationMs;
  if (!trailerSeeking && trailerDurationMs > 0) {
    els['trailer-seek'].value = Math.round((status.positionMs / trailerDurationMs) * 1000);
  }
  els['trailer-time'].textContent = `${formatTime(status.positionMs)} / ${
    trailerDurationMs ? formatTime(trailerDurationMs) : '--:--'
  }`;
  // 播完自动收起，避免停在黑屏上等 5 分钟兜底
  if (trailerDurationMs > 0 && status.positionMs >= trailerDurationMs - 300) {
    stopPreview();
  }
}

/** 指针已不在预览舞台、也不在触发预览的卡片内（画面窗口挡住了指针，重新收到事件即意味着离开了）。 */
function pointerOutsidePreview(event) {
  if (!previewRect) {
    return false;
  }
  const px = toPhysical(event.clientX);
  const py = toPhysical(event.clientY);
  const tol = 2;
  const inRect = (rect) =>
    !!rect &&
    px >= rect.x - tol &&
    py >= rect.y - tol &&
    px <= rect.x + rect.w + tol &&
    py <= rect.y + rect.h + tol;
  if (inRect(previewRect)) {
    return false;
  }
  // 放大舞台下方还有一段 HTML 控制条（声音 / 收起），介于画面与按钮之间的
  // 间隙也算交互区，否则指针移向按钮的途中预览就被收起了。
  const stage = els['preview-stage'];
  if (stage && !stage.classList.contains('hidden')) {
    const frame = els['preview-frame'];
    const controls = els['preview-controls'];
    if (frame && controls) {
      const fr = frame.getBoundingClientRect();
      const cr = controls.getBoundingClientRect();
      const left = Math.min(fr.left, cr.left);
      const right = Math.max(fr.right, cr.right);
      const zone = {
        x: toPhysical(left),
        y: toPhysical(fr.top),
        w: toPhysical(right - left),
        h: toPhysical(cr.bottom - fr.top),
      };
      if (inRect(zone)) {
        return false;
      }
    }
  }
  if (previewAnchorRect) {
    const ax = toPhysical(previewAnchorRect.left);
    const ay = toPhysical(previewAnchorRect.top);
    const aw = toPhysical(previewAnchorRect.width);
    const ah = toPhysical(previewAnchorRect.height);
    if (inRect({ x: ax, y: ay, w: aw, h: ah })) {
      return false;
    }
  }
  return true;
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
  playerReserveCss = 0;
  currentDurationMs = status.durationMs || 0;
  lastPositionMs = status.positionMs || 0;
  els['player-settings'].classList.add('hidden');
  els['pb-title'].textContent = item ? item.title : (items.find((i) => i.id === itemId) || {}).title || '—';
  setPlayIcon(false);
  els['pb-seek'].value = 0;
  updateTimeUi(status.positionMs || 0, currentDurationMs);
  els['player-backdrop'].classList.remove('hidden');
  els['player-bar'].classList.remove('hidden');
  playerBarHidden = false;
  els['player-bar'].classList.remove('bar-auto-hidden');
  playerSentReserve = 0;
  applyView();
  startTick();
  // 画面比例决定字幕应贴在画面内底部还是窗口底部，首帧后读取
  watchVideoAspect();
  // 打开成功后取入口可见性 + 已存轨道：有外挂/生成轨道时按其句时间轴复用渲染
  await initSubtitleForItem(itemId);
  revealPlayerBar();
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
  lastPositionMs = status.positionMs || 0;
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

// ---- 播放器设置：快进 / 预览偏好（localStorage，本机生效） ----

const FORWARD_KEY = 'cinema.forwardSeconds';
const PREVIEW_DELAY_KEY = 'cinema.previewDelaySeconds';
const PREVIEW_SOUND_KEY = 'cinema.previewSound';

function readNumberPref(key, fallback, min, max) {
  let value = fallback;
  try {
    const raw = Number(localStorage.getItem(key));
    if (Number.isFinite(raw) && raw > 0) value = raw;
  } catch (err) {
    /* 存储不可用时用默认值 */
  }
  return Math.min(max, Math.max(min, Math.round(value)));
}

function readBoolPref(key, fallback) {
  try {
    const raw = localStorage.getItem(key);
    return raw === null ? fallback : raw === '1';
  } catch (err) {
    return fallback;
  }
}

let forwardSeconds = readNumberPref(FORWARD_KEY, 10, 1, 600);
let previewDelaySeconds = readNumberPref(PREVIEW_DELAY_KEY, 5, 1, 60);
let previewSoundOn = readBoolPref(PREVIEW_SOUND_KEY, false);

function previewDelayMs() {
  return previewDelaySeconds * 1000;
}

function fillPlayerSettings() {
  els['pset-forward'].value = String(forwardSeconds);
  els['pset-preview-delay'].value = String(previewDelaySeconds);
  els['pset-preview-sound'].checked = previewSoundOn;
}

function savePlayerSettings() {
  const fwd = Number(els['pset-forward'].value);
  const delay = Number(els['pset-preview-delay'].value);
  if (!Number.isFinite(fwd) || fwd < 1 || fwd > 600) {
    showToast('快进时长需在 1–600 秒之间');
    return;
  }
  if (!Number.isFinite(delay) || delay < 1 || delay > 60) {
    showToast('预览停留需在 1–60 秒之间');
    return;
  }
  forwardSeconds = Math.round(fwd);
  previewDelaySeconds = Math.round(delay);
  previewSoundOn = !!els['pset-preview-sound'].checked;
  try {
    localStorage.setItem(FORWARD_KEY, String(forwardSeconds));
    localStorage.setItem(PREVIEW_DELAY_KEY, String(previewDelaySeconds));
    localStorage.setItem(PREVIEW_SOUND_KEY, previewSoundOn ? '1' : '0');
  } catch (err) {
    /* 存储不可用时仅本次生效 */
  }
  closePlayerSettings();
  showToast('播放器设置已保存');
}

function openPlayerSettings() {
  fillPlayerSettings();
  revealPlayerBar();
  els['player-settings'].classList.remove('hidden');
  syncPlayerPanels();
}

function closePlayerSettings() {
  if (els['player-settings'].classList.contains('hidden')) return;
  els['player-settings'].classList.add('hidden');
  syncPlayerPanels();
}

// ---- 控制条自动隐藏：鼠标静止 3 秒后收起，画面铺满窗口；移动立即唤醒 ----

function revealPlayerBar() {
  if (!playingId) return;
  if (playerBarHidden) {
    playerBarHidden = false;
    els['player-bar'].classList.remove('bar-auto-hidden');
    syncPlayerPanels();
  }
  schedulePlayerBarHide();
  updateSubtitleStage();
}

function schedulePlayerBarHide() {
  clearTimeout(playerBarHideTimer);
  playerBarHideTimer = null;
  if (!playingId || inMulti) return;
  playerBarHideTimer = setTimeout(() => {
    playerBarHideTimer = null;
    if (!playingId || inMulti) return;
    // 面板还开着时保持控制条可见，等面板收起后重新计时
    if (panelReserveCss() > 0) {
      schedulePlayerBarHide();
      return;
    }
    playerBarHidden = true;
    // 收起本身会撑开原生画面窗口，可能引发一批「位置没变」的移动事件；
    // 短暂屏蔽这些事件，避免收起→唤醒→再收起的死循环（表现为画面闪动、控制条不消失）
    playerBarWakeBlockUntil = Date.now() + 700;
    els['player-bar'].classList.add('bar-auto-hidden');
    updateSubtitleStage();
    syncPlayerPanels();
  }, PLAYER_BAR_IDLE_MS);
}

async function forwardBy(seconds) {
  if (!playingId || inMulti) return;
  const delta = Math.round(Number(seconds) * 1000);
  if (!delta) return;
  const base = Math.max(0, lastPositionMs || 0);
  let target = base + delta;
  if (currentDurationMs > 0) {
    // 贴近片尾时停在片尾前，避免 seek 到总时长直接触发结束
    target = Math.min(target, Math.max(base, currentDurationMs - 500));
  }
  try {
    await invoke('seek', { positionMs: target });
  } catch (err) {
    showToast(String(err));
    return;
  }
  lastPositionMs = target;
  if (currentDurationMs > 0) {
    els['pb-seek'].value = Math.round((target / currentDurationMs) * 1000);
  }
  updateTimeUi(target, currentDurationMs);
  // seek 丢弃旧句：清空本地累积的句子与复用轨道当前句（与拖动进度条一致）
  resetSubtitleText(null);
  renderSubtitleLines(null);
}

const ICON_PAUSE =
  '<svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true"><rect x="6" y="4" width="4" height="16" rx="1"></rect><rect x="14" y="4" width="4" height="16" rx="1"></rect></svg>';
const ICON_PLAY =
  '<svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true"><path d="M7 4.5v15l13-7.5z"></path></svg>';

/** 播放/暂停按钮用图标表达：两根竖线=暂停中，三角=播放中。 */
function setPlayIcon(isPaused) {
  els['pb-toggle'].innerHTML = isPaused ? ICON_PLAY : ICON_PAUSE;
  els['pb-toggle'].title = isPaused ? '播放' : '暂停';
}

async function togglePause() {
  if (!playingId) {
    return;
  }
  paused = !paused;
  setPlayIcon(paused);
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
  playerReserveCss = 0;
  currentDurationMs = 0;
  lastPositionMs = 0;
  lastFault = null;
  els['player-backdrop'].classList.add('hidden');
  els['player-bar'].classList.add('hidden');
  els['player-settings'].classList.add('hidden');
  clearTimeout(playerBarHideTimer);
  playerBarHideTimer = null;
  playerBarHidden = false;
  els['player-bar'].classList.remove('bar-auto-hidden');
  playerSentReserve = 0;
  resetVideoAspect();
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
  // 主机模式播放中退出（窗口尚未还原）时，等播放页关闭后再把窗口还原
  if (!consoleOn) {
    setConsoleWindowMode(false);
  }
}

// ---------------------------------------------------------------------------
// 入库（PRD FR-01）
// ---------------------------------------------------------------------------

// 文件夹选择：由 tauri-plugin-dialog 提供原生对话框，用户直接选目录而不必复制粘贴路径。
// 插件未就绪（或非 Tauri 环境）时返回 null，调用方放弃本次操作。
async function pickFolder(title) {
  const dialog = TAURI && TAURI.dialog;
  if (!dialog || typeof dialog.open !== 'function') {
    showToast('当前环境不支持文件夹选择');
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

// 盘根判定：`E:` / `E:\` 都算「整个磁盘」。选中盘根会扫描该盘上所有文件夹（含系统目录），
// 既慢又会把无关视频混进库里，所以入库前必须让用户确认一次。
function isDriveRoot(path) {
  return /^[A-Za-z]:(\\|\/)?$/.test(String(path).trim());
}

/** 二次确认：优先用系统对话框，插件不可用时退回浏览器确认框。 */
async function confirmDialog(title, message) {
  const dialog = TAURI && TAURI.dialog;
  if (dialog && typeof dialog.ask === 'function') {
    try {
      return await dialog.ask({ title, kind: 'warning', message });
    } catch (err) {
      /* 对话框不可用时退回浏览器确认框 */
    }
  }
  return window.confirm(message);
}

async function confirmWholeDrive(path) {
  return confirmDialog(
    '扫描整个磁盘？',
    `你选择的是整个磁盘 ${path}，将扫描该盘上的全部文件夹，可能非常慢。确定继续吗？`,
  );
}

// 「添加目录」是唯一的入库入口：点按钮 → 系统文件夹选择器 → 确认后先入库该目录，
// 再只扫描这一个源。不再提供独立的「扫描入库」（那会把库里所有源（含旧的整盘源）
// 一起重扫，用户看到的却是「怎么又在扫整个磁盘」）。
async function addSource() {
  const path = await pickFolder('选择要入库的本地目录');
  if (!path) {
    return; // 用户取消选择：什么都不做
  }
  if (isDriveRoot(path) && !(await confirmWholeDrive(path))) {
    return;
  }
  els['add-btn'].disabled = true;
  try {
    const source = await invoke('add_local_source', { path });
    await refreshSources();
    await scanSource(source);
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

// 扫描单个源（自动扫描与后续重扫共用同一条路径）。
async function scanSource(source) {
  if (!source) {
    return;
  }
  openScanOverlay();
  scanTargetName = source.name;
  // 先挂监听再发命令：后端在命令入口就会推第一条进度，晚了会丢掉这次起点
  const unlisten = listen
    ? await listen('scan-progress', (event) => renderScanProgress(event.payload))
    : null;
  try {
    const report = await invoke('scan_source', { id: source.id });
    if (report.cancelled) {
      showToast(`已取消扫描：本次入库 ${report.stored} 条`);
    } else {
      showToast(
        `${source.name} 扫描完成：入库 ${report.stored} 条` +
          (report.skipped ? `，跳过 ${report.skipped} 项` : ''),
      );
    }
    await refresh();
    // 入库会生成派生标签（时长区间 / 画质 / 字幕状态），取值清单随之变化
    await reloadFilters();
  } catch (err) {
    showToast(`${source.name} 扫描失败：${err}`);
  } finally {
    if (unlisten) {
      unlisten();
    }
    closeScanOverlay();
  }
}

// ---------------------------------------------------------------------------
// 元数据刮削（P-06）：数据源设置 / 条目详情 / 预告片 / 批量补齐
// ---------------------------------------------------------------------------

let detailItem = null;
/** 详情请求序号：重开或重抓时丢弃过期响应，避免旧数据覆盖新画面。 */
let detailSeq = 0;

/** 用户点「补齐全库」触发的批处理：用于把 scrape-progress 事件只落到这条进度条上。 */
let scrapeBatchActive = false;
let scrapeBatchDone = 0;
let scrapeBatchTotal = 0;

async function openMetaModal() {
  els['meta-modal'].classList.remove('hidden');
  els['meta-cookie'].value = '';
  els['meta-status'].textContent = '正在读取数据源设置…';
  // 上次补齐结果不跨次保留：非补齐中时清空进度块
  if (!scrapeBatchActive) {
    els['meta-progress'].classList.add('hidden');
    els['meta-progress-fill'].style.width = '0%';
    els['meta-progress-pct'].textContent = '0%';
    els['meta-progress-current'].textContent = '';
  }
  try {
    const view = await invoke('javdb_settings');
    applyMetaView(view);
  } catch (err) {
    els['meta-status'].textContent = String(err);
  }
  try {
    applyLlmView(await invoke('llm_settings'));
  } catch (err) {
    els['llm-status'].textContent = String(err);
  }
}

/** 推荐行的快捷入口：退出主机模式，打开「元数据」并聚焦大模型密钥输入。 */
async function openLlmSettings() {
  if (consoleOn) {
    exitConsole();
  }
  await openMetaModal();
  const key = els['llm-key'];
  if (key) {
    key.scrollIntoView({ block: 'center' });
    key.focus();
  }
}

/** 预置服务商：接口地址与可选模型由程序固化，用户只需选服务商 + 填密钥。 */
const LLM_PROVIDERS = {
  deepseek: {
    baseUrl: 'https://api.deepseek.com/v1',
    models: ['deepseek-chat', 'deepseek-reasoner'],
  },
  openai: {
    baseUrl: 'https://api.openai.com/v1',
    models: ['gpt-4o-mini', 'gpt-4o'],
  },
};

/** 切换服务商：预置服务商自动填地址、给出模型下拉；「自定义」才显示手填项。 */
function applyLlmProvider(provider, model) {
  const preset = LLM_PROVIDERS[provider];
  els['llm-provider'].value = preset ? provider : 'custom';
  els['llm-custom'].classList.toggle('hidden', !!preset);
  els['llm-model-preset'].classList.toggle('hidden', !preset);
  const select = els['llm-provider-model'];
  select.replaceChildren();
  if (!preset) {
    return;
  }
  els['llm-base'].value = preset.baseUrl;
  const models = preset.models.slice();
  if (model && !models.includes(model)) {
    models.push(model);
  }
  for (const name of models) {
    const option = document.createElement('option');
    option.value = name;
    option.textContent = name;
    select.appendChild(option);
  }
  select.value = model && models.includes(model) ? model : preset.models[0];
}

/** 大模型配置回填：只回报是否已保存密钥，不回显密钥本身。 */
function applyLlmView(view) {
  els['llm-base'].value = view.baseUrl || '';
  els['llm-model'].value = view.model || '';
  els['llm-key'].value = '';
  els['llm-key'].placeholder = view.hasKey ? '已保存密钥（留空表示保持不变）' : '粘贴服务商的 API Key';
  applyLlmProvider(view.provider || 'deepseek', view.model);
  els['llm-status'].textContent = view.hasKey
    ? '已配置密钥：推荐看点由大模型总结'
    : '未配置密钥：推荐看点按高赞评论本地提炼';
}

function applyMetaView(view) {
  els['meta-base'].value = view.baseUrl || '';
  els['meta-cookie'].value = '';
  els['meta-cookie'].placeholder = view.hasCookie
    ? '已保存 Cookie（留空表示保持不变）'
    : '粘贴浏览器请求头里的 Cookie 整行';
  const who = view.username ? `（${view.username}）` : '';
  els['meta-status'].textContent = view.message || (view.loggedIn ? `已登录${who}` : '未登录');
}

async function saveMetaSettings() {
  els['meta-save'].disabled = true;
  els['meta-status'].textContent = '正在验证…';
  try {
    const view = await invoke('save_javdb_settings', {
      baseUrl: els['meta-base'].value,
      cookie: els['meta-cookie'].value,
    });
    applyMetaView(view);
    showToast(view.loggedIn ? `数据源已连接${view.username ? `：${view.username}` : ''}` : '设置已保存');
  } catch (err) {
    els['meta-status'].textContent = String(err);
    showToast(String(err));
  }
  // 推荐看点的大模型配置与数据源一起保存；密钥留空表示保留已保存的值
  try {
    const provider = els['llm-provider'].value;
    const llmView = await invoke('save_llm_settings', {
      provider,
      baseUrl: els['llm-base'].value,
      apiKey: els['llm-key'].value,
      model: provider === 'custom'
        ? els['llm-model'].value
        : els['llm-provider-model'].value,
    });
    applyLlmView(llmView);
    // 配置完成后立刻在后台拉取首批推荐：回到主机模式即可直接看到，不必再点「换一批」
    if (llmView.hasKey) {
      consoleRecLoaded = false;
      loadConsoleRecommendations();
    }
  } catch (err) {
    els['llm-status'].textContent = String(err);
  } finally {
    els['meta-save'].disabled = false;
  }
}

/** 进度事件 → 进度条：总数、已处理、百分比与当前条目标题。 */
function updateScrapeProgress(payload) {
  if (!scrapeBatchActive) {
    return;
  }
  const total = Number(payload.total) || 0;
  const done = Number(payload.done) || 0;
  scrapeBatchDone = done;
  scrapeBatchTotal = total;
  const pct = total > 0 ? Math.min(100, Math.round((done / total) * 100)) : 0;
  els['meta-progress'].classList.remove('hidden');
  els['meta-progress-fill'].style.width = `${pct}%`;
  els['meta-progress-pct'].textContent = `${pct}%`;
  els['meta-progress-text'].textContent = total
    ? `正在补齐元数据：${done} / ${total}`
    : '正在读取待补齐条目…';
  const item = consoleItemById(payload.itemId) || items.find((i) => i.id === payload.itemId);
  els['meta-progress-current'].textContent = item ? item.title : payload.itemId || '';
}

/** 批量补齐：不指定源则补齐全库中待刮削 / 上次失败的条目；批处理期间显示进度条。 */
async function scrapeMissing(sourceId) {
  if (scrapeBatchActive) {
    return;
  }
  scrapeBatchActive = true;
  scrapeBatchDone = 0;
  scrapeBatchTotal = 0;
  els['meta-progress'].classList.remove('hidden');
  els['meta-progress-fill'].style.width = '0%';
  els['meta-progress-pct'].textContent = '0%';
  els['meta-progress-text'].textContent = '正在读取待补齐条目…';
  els['meta-progress-current'].textContent = '';
  els['meta-scrape-all'].disabled = true;
  els['meta-scrape-all'].textContent = '补齐中…';
  let aborted = false;
  try {
    const count = await invoke('scrape_missing', sourceId ? { sourceId } : {});
    coverCache.clear();
    showToast(count ? `已刮削 ${count} 条元数据` : '没有待刮削的条目');
    await refresh();
    await reloadFilters();
  } catch (err) {
    aborted = true;
    showToast(String(err));
    els['meta-progress-text'].textContent = '补齐中止';
  } finally {
    scrapeBatchActive = false;
    els['meta-scrape-all'].disabled = false;
    els['meta-scrape-all'].textContent = '补齐全库';
    // 收尾：成功时把条推到 100%，失败则保留已跑到位置供排查
    if (!aborted) {
      els['meta-progress-fill'].style.width = '100%';
      els['meta-progress-pct'].textContent = '100%';
      els['meta-progress-text'].textContent = scrapeBatchTotal
        ? `补齐完成 · 已处理 ${scrapeBatchDone} / ${scrapeBatchTotal}`
        : '没有待补齐的条目';
      els['meta-progress-current'].textContent = '';
    }
  }
}

function scrapeStateText(data) {
  switch (data.scrapeState) {
    case 'done':
      return '元数据已从数据源同步';
    case 'running':
      return '正在刮削…';
    case 'failed':
      return `上次刮削失败：${data.scrapeError || '未知原因'}`;
    case 'no_code':
      return `未能识别番号：${data.scrapeError || '请重命名文件后重试'}`;
    default:
      return '尚未刮削：点「从数据源更新」按番号抓取';
  }
}

async function openDetail(item) {
  stopPreview();
  detailItem = item;
  const seq = (detailSeq += 1);
  els['detail-modal'].classList.remove('hidden');
  els['detail-title'].textContent = item.title;
  els['detail-meta'].textContent = '正在读取元数据…';
  els['detail-state'].textContent = '';
  els['detail-trailer'].disabled = true;
  els['detail-poster'].replaceChildren();
  els['detail-tags'].replaceChildren();
  els['detail-shots'].replaceChildren();
  els['detail-comments'].replaceChildren();
  await loadDetail(item.id, seq);
}

function hideDetail() {
  els['detail-modal'].classList.add('hidden');
  detailItem = null;
  detailSeq += 1;
  stopPreview();
}

/** 标签跳转前保存的页面状态：返回时原样恢复（含主机模式焦点与滚动位置）。 */
let tagNav = null;

/** 详情页标签 → 该标签下的影片列表；「← 返回」回到详情页，原列表状态不丢。 */
async function openTagBrowse(label, value) {
  const dim = dims.find((d) => d.label === label);
  const screenEl = consoleOn
    ? (consoleScreen === 'home' ? els['c-screen-home'] : els['c-screen-lib'])
    : null;
  tagNav = {
    item: detailItem,
    selected: JSON.parse(JSON.stringify(selected)),
    searchText,
    sortTag,
    wallScroll: els.wall.scrollTop,
    console: consoleOn,
    consoleScreen,
    focus: { ...consoleFocus },
    consoleScroll: screenEl ? screenEl.scrollTop : 0,
  };
  hideDetail();
  if (consoleOn) {
    exitConsole();
  }
  if (dim) {
    // 精确按该维度筛选；清空其它条件，避免「查不到」的困惑
    selected = { [dim.id]: [value] };
    searchText = '';
  } else {
    selected = {};
    searchText = value;
  }
  els['search-input'].value = searchText;
  els['sort-select'].value = sortTag;
  renderFilterBar();
  renderDims();
  els['tag-back'].classList.remove('hidden');
  await refresh();
  els.wall.scrollTop = 0;
}

async function backFromTagBrowse() {
  if (!tagNav) {
    return;
  }
  const saved = tagNav;
  tagNav = null;
  selected = saved.selected;
  searchText = saved.searchText;
  sortTag = saved.sortTag;
  els['search-input'].value = searchText;
  els['sort-select'].value = sortTag;
  renderFilterBar();
  renderDims();
  els['tag-back'].classList.add('hidden');
  await refresh();
  els.wall.scrollTop = saved.wallScroll;
  if (saved.console) {
    consoleFocus = saved.focus;
    await enterConsole();
    // enterConsole 默认回首页；原来是资源库时切回去并恢复焦点
    if (saved.consoleScreen !== 'home') {
      consoleScreen = saved.consoleScreen;
      consoleFocus = saved.focus;
      renderConsole();
    }
    const screenEl = saved.consoleScreen === 'home' ? els['c-screen-home'] : els['c-screen-lib'];
    screenEl.scrollTop = saved.consoleScroll;
  }
  // 回到详情页本身，用户可继续看元数据，再点 × 回到原列表
  if (saved.item) {
    openDetail(saved.item);
  }
}

async function loadDetail(itemId, seq) {
  let data;
  try {
    data = await invoke('item_detail', { itemId });
  } catch (err) {
    if (seq === detailSeq) {
      els['detail-meta'].textContent = String(err);
    }
    return;
  }
  if (seq !== detailSeq) {
    return;
  }

  els['detail-title'].textContent = data.title || '—';
  const meta = [];
  if (data.code) meta.push(`番号 ${data.code}`);
  if (data.releaseDate) meta.push(`发行 ${data.releaseDate}`);
  if (data.durationMs) meta.push(`时长 ${formatTime(data.durationMs)}`);
  if (data.rating) meta.push(`评分 ${Number(data.rating).toFixed(1)}`);
  els['detail-meta'].textContent = meta.join(' · ') || '暂无发行信息';

  if (data.hasPoster) {
    const img = document.createElement('img');
    img.alt = data.title || '封面';
    img.addEventListener('click', () => openLightbox(img.src));
    els['detail-poster'].appendChild(img);
    const cached = coverCache.get(itemId);
    if (cached) {
      img.src = cached;
    } else {
      invoke('item_cover', { itemId })
        .then((src) => {
          if (src && seq === detailSeq) {
            coverCache.set(itemId, src);
            img.src = src;
          }
        })
        .catch(() => {});
    }
  } else {
    els['detail-poster'].appendChild(el('div', 'detail-poster-empty', '暂无封面'));
  }

  const groups = [
    ['类别', data.categories],
    ['系列', data.series],
    ['片商', data.maker],
    ['导演', data.director],
    ['演员', data.actors],
  ];
  for (const [label, values] of groups) {
    if (!Array.isArray(values) || !values.length) continue;
    const row = el('div', 'detail-tag-row');
    row.appendChild(el('span', 'dt-label', label));
    for (const value of values) {
      const chip = el('span', 'chip dt-chip-link', value);
      chip.title = `查看「${label} · ${value}」下的影片`;
      chip.addEventListener('click', () => openTagBrowse(label, value));
      row.appendChild(chip);
    }
    els['detail-tags'].appendChild(row);
  }
  if (!els['detail-tags'].childElementCount) {
    els['detail-tags'].appendChild(el('div', 'detail-meta', '暂无标签：点「从数据源更新」抓取'));
  }

  if (Array.isArray(data.screenshots) && data.screenshots.length) {
    for (const name of data.screenshots) {
      const img = document.createElement('img');
      img.alt = '截图';
      img.addEventListener('click', () => openLightbox(img.src));
      els['detail-shots'].appendChild(img);
      invoke('item_screenshot', { itemId, fileName: name })
        .then((src) => {
          if (src && seq === detailSeq) {
            img.src = src;
          } else {
            img.remove();
          }
        })
        .catch(() => img.remove());
    }
  } else {
    els['detail-shots'].appendChild(el('div', 'detail-meta', '暂无截图'));
  }

  if (Array.isArray(data.comments) && data.comments.length) {
    for (const comment of data.comments) {
      const box = el('div', 'detail-comment');
      const head = el('div', 'detail-comment-head');
      head.appendChild(el('span', 'detail-comment-author', comment.author || '匿名'));
      if (comment.score) {
        head.appendChild(el('span', 'detail-comment-score', `${comment.score} 星`));
      }
      head.appendChild(el('span', 'detail-comment-likes', `赞 ${comment.likes || 0}`));
      if (comment.postedAt) {
        head.appendChild(el('span', 'detail-comment-likes', comment.postedAt));
      }
      box.appendChild(head);
      box.appendChild(el('div', 'detail-comment-body', comment.content));
      els['detail-comments'].appendChild(box);
    }
  } else {
    els['detail-comments'].appendChild(
      el('div', 'detail-meta', '暂无短评：需在「元数据」里填入登录 Cookie 后才会抓取'),
    );
  }

  els['detail-state'].textContent = scrapeStateText(data);
  els['detail-trailer'].disabled = !data.hasTrailer;
  els['detail-trailer'].title = data.hasTrailer ? '' : '还没有预告片，点「从数据源更新」抓取';
}

/** 当前详情条目重新从数据源抓取（手动触发，force = true）。 */
async function scrapeCurrentItem() {
  if (!detailItem) {
    return;
  }
  const item = detailItem;
  els['detail-refresh'].disabled = true;
  els['detail-state'].textContent = '正在从数据源抓取…';
  try {
    const result = await invoke('scrape_item', { itemId: item.id, force: true });
    coverCache.delete(item.id);
    if (result.message) {
      els['detail-state'].textContent = `刮削失败：${result.message}`;
      showToast(`刮削失败：${result.message}`);
      return;
    }
    showToast(
      `已更新：截图 ${result.screenshots} 张 · 短评 ${result.comments} 条` +
        (result.trailerSaved ? ' · 预告片已保存' : ''),
    );
    await loadDetail(item.id, (detailSeq += 1));
    await refresh();
    await reloadFilters();
  } catch (err) {
    els['detail-state'].textContent = String(err);
    showToast(String(err));
  } finally {
    els['detail-refresh'].disabled = false;
  }
}

/** 在详情封面位置播放预告片（原生预览窗口覆盖封面区域）。 */
async function playDetailTrailer() {
  if (!detailItem) {
    return;
  }
  stopPreview();
  els['trailer-stage'].classList.remove('hidden');
  const rect = els['trailer-frame'].getBoundingClientRect();
  const x = toPhysical(rect.left);
  const y = toPhysical(rect.top);
  const width = toPhysical(rect.width);
  const height = toPhysical(rect.height);
  try {
    els['detail-state'].textContent = '正在准备预告片…';
    const result = await invoke('play_trailer', { itemId: detailItem.id, x, y, width, height });
    if (!result.started) {
      els['trailer-stage'].classList.add('hidden');
      els['detail-state'].textContent = result.reason || '无法播放预告片';
      showToast(result.reason || '无法播放预告片');
      return;
    }
    els['detail-state'].textContent = '预告片播放中：点击画面或关闭详情即可停止';
    previewItemId = detailItem.id;
    previewRect = { x, y, w: width, h: height };
    startTrailerTick();
    clearTimeout(previewStopTimer);
    // 预告片通常 1~3 分钟：超时兜底，点击画面 / 关闭详情都会立即停止
    previewStopTimer = setTimeout(stopPreview, 5 * 60 * 1000);
  } catch (err) {
    showToast(String(err));
  }
}

/** 放大查看封面 / 截图：全屏遮罩层，点击任意处关闭。 */
function openLightbox(src) {
  if (!src) {
    return;
  }
  els['lightbox-img'].src = src;
  els['lightbox'].classList.remove('hidden');
}

function closeLightbox() {
  els['lightbox'].classList.add('hidden');
  els['lightbox-img'].removeAttribute('src');
}

/** 从详情进入正式播放：先收起详情，避免与续播弹层叠在一起。 */
function playFromDetail() {
  if (!detailItem) {
    return;
  }
  const item = detailItem;
  hideDetail();
  openItem(item);
}

// ---------------------------------------------------------------------------
// 库管理：左栏媒体源列表 + 右栏选中源的详情与操作（只删库内记录，不删硬盘文件）
// ---------------------------------------------------------------------------

/** 媒体源的扫描状态：文案 + 色调，列表圆点与详情徽标共用。 */
const MANAGE_STATES = {
  idle: { text: '正常', tone: 'ok' },
  scanning: { text: '扫描中', tone: 'busy' },
  scan_failed: { text: '扫描失败', tone: 'bad' },
};

let manageSources = [];        // 最近一次 list_sources 的结果
let manageSelectedId = null;   // 右栏当前展示的源；重新渲染时优先保留

function manageStateOf(source) {
  return MANAGE_STATES[source.state] || MANAGE_STATES.idle;
}

async function openManageModal() {
  els['manage-list'].replaceChildren();
  els['manage-detail'].replaceChildren();
  els['manage-modal'].classList.remove('hidden');
  await renderManageList();
}

function hideManageModal() {
  els['manage-modal'].classList.add('hidden');
}

/** 拉取全部媒体源并重绘两栏；选中项失效（被清除）时退回第一个。 */
async function renderManageList() {
  let list = [];
  try {
    list = await invoke('list_sources');
  } catch (err) {
    showToast(String(err));
    return;
  }
  manageSources = Array.isArray(list) ? list : [];
  if (!manageSources.some((source) => source.id === manageSelectedId)) {
    manageSelectedId = manageSources.length ? manageSources[0].id : null;
  }
  renderManageSidebar();
  renderManageDetail();
}

/** 顶栏汇总：源数 / 条目总数 / 合并目录数 / 失败源数。 */
function renderManageSummary() {
  const box = els['manage-summary'];
  box.replaceChildren();
  const items = manageSources.reduce((sum, source) => sum + (source.itemCount || 0), 0);
  const merged = manageSources.reduce(
    (sum, source) => sum + ((source.extraRoots || []).length),
    0,
  );
  const failed = manageSources.filter((source) => source.state === 'scan_failed').length;

  const chip = (text, tone) => el('span', 'manage-chip' + (tone ? ` ${tone}` : ''), text);
  box.append(chip(`源 ${manageSources.length}`), chip(`条目 ${items.toLocaleString('zh-CN')}`));
  if (merged) {
    box.appendChild(chip(`合并目录 ${merged}`));
  }
  if (failed) {
    box.appendChild(chip(`失败 ${failed}`, 'bad'));
  }
  els['manage-source-count'].textContent = String(manageSources.length);
}

function renderManageSidebar() {
  renderManageSummary();
  const list = els['manage-list'];
  list.replaceChildren();
  if (!manageSources.length) {
    list.appendChild(el('div', 'manage-list-empty', '尚未添加媒体源'));
    return;
  }
  for (const source of manageSources) {
    const selected = source.id === manageSelectedId;
    const state = manageStateOf(source);

    const row = el('button', 'manage-item' + (selected ? ' on' : ''));
    row.type = 'button';
    row.setAttribute('role', 'option');
    row.setAttribute('aria-selected', selected ? 'true' : 'false');
    row.title = `${source.name}（${source.location}）`;

    const top = el('span', 'mi-top');
    const dot = el('span', `mi-dot ${state.tone}`);
    dot.title = state.text;
    top.append(dot, el('span', 'mi-name', source.name), el('span', 'mi-count', `${source.itemCount} 条`));
    row.appendChild(top);
    row.appendChild(el('span', 'mi-path', source.location));
    if (state.tone === 'bad') {
      const err = el('span', 'mi-err', source.lastError || '最近一次扫描失败');
      err.title = source.lastError || '最近一次扫描失败';
      row.appendChild(err);
    }

    row.addEventListener('click', () => {
      if (manageSelectedId === source.id) {
        return;
      }
      manageSelectedId = source.id;
      renderManageSidebar();
      renderManageDetail();
    });
    list.appendChild(row);
  }
}

/** 右栏：选中源的路径、条目数、合并目录与操作。无源时给空状态引导。 */
function renderManageDetail() {
  const box = els['manage-detail'];
  box.replaceChildren();

  const source = manageSources.find((item) => item.id === manageSelectedId);
  if (!source) {
    const empty = el('div', 'manage-empty');
    empty.append(
      el('div', 'manage-empty-mark', '库'),
      el('div', 'manage-empty-title', '还没有媒体源'),
      el('div', 'manage-empty-sub', '回到顶栏点「添加目录」，选择本机文件夹后会自动扫描入库。'),
    );
    box.appendChild(empty);
    return;
  }

  const state = manageStateOf(source);

  const head = el('div', 'md-head');
  head.appendChild(el('h3', 'md-name', source.name));
  const pill = el('span', `md-state ${state.tone}`, state.text);
  if (source.lastError) {
    pill.title = source.lastError;
  }
  head.appendChild(pill);
  box.appendChild(head);

  const path = el('div', 'md-path');
  path.appendChild(el('span', 'md-path-label', '目录'));
  path.appendChild(el('span', 'md-path-text', source.location));
  box.appendChild(path);

  if (source.state === 'scan_failed') {
    box.appendChild(
      el('div', 'md-alert', `上次扫描失败：${source.lastError || '未知原因'}`),
    );
  }

  const extraRoots = source.extraRoots || [];
  const facts = el('div', 'md-facts');
  const fact = (label, value) => {
    const card = el('div', 'md-fact');
    card.append(el('div', 'md-fact-label', label), el('div', 'md-fact-value', value));
    return card;
  };
  facts.append(
    fact('条目数', String(source.itemCount)),
    fact('合并目录', String(extraRoots.length)),
  );
  box.appendChild(facts);

  // 合并目录：只登记不移动文件，每个可单独解除
  const rootsSec = el('section', 'md-section');
  const rootsHead = el('div', 'md-section-head');
  rootsHead.append(
    el('span', 'md-section-title', '合并目录'),
    el('span', 'md-section-note', extraRoots.length ? '只登记路径，不移动文件' : '未合并其它目录'),
  );
  rootsSec.appendChild(rootsHead);
  if (extraRoots.length) {
    const roots = el('div', 'md-roots');
    for (const extra of extraRoots) {
      const row = el('div', 'md-root');
      const text = el('span', 'md-root-text', extra);
      text.title = extra;
      const detach = el('button', 'md-root-x', '×');
      detach.type = 'button';
      detach.title = '取消该目录与本源的合并，并清除其已入库条目（不删除硬盘文件）';
      detach.addEventListener('click', () => detachRoot(source, extra));
      row.append(text, detach);
      roots.appendChild(row);
    }
    rootsSec.appendChild(roots);
  }
  box.appendChild(rootsSec);

  // 入库：扫描主目录与合并目录、合并新目录、登记单个视频文件
  const intakeSec = el('section', 'md-section');
  intakeSec.appendChild(el('div', 'md-section-title', '入库'));
  const intake = el('div', 'md-actions');

  const scan = el('button', 'btn primary', '扫描新视频');
  scan.type = 'button';
  scan.title = '重新扫描该目录（含合并目录），把新增视频入库';
  scan.addEventListener('click', () => rescanSource(source));

  const merge = el('button', 'btn', '合并目录');
  merge.type = 'button';
  merge.title = '把另一个目录登记到本媒体源（只登记，不移动文件）';
  merge.addEventListener('click', () => mergeDirectory(source));

  const addVideos = el('button', 'btn', '添加视频');
  addVideos.type = 'button';
  addVideos.title = '选择视频文件登记到本媒体源（只登记，不移动文件）';
  addVideos.addEventListener('click', () => addVideosToSource(source));

  intake.append(scan, merge, addVideos);
  intakeSec.appendChild(intake);
  box.appendChild(intakeSec);

  // 维护：按番号补齐全源元数据
  const maintainSec = el('section', 'md-section');
  const maintainHead = el('div', 'md-section-head');
  maintainHead.append(
    el('span', 'md-section-title', '维护'),
    el('span', 'md-section-note', '按番号抓取标题 / 封面 / 标签 / 短评'),
  );
  const maintain = el('div', 'md-actions');
  const fill = el('button', 'btn', '补齐元数据');
  fill.type = 'button';
  fill.disabled = !source.itemCount;
  fill.title = '按番号从数据源抓取标题 / 封面 / 截图 / 标签 / 日期 / 演员 / 短评 / 预告片';
  fill.addEventListener('click', () => scrapeMissing(source.id));
  maintain.appendChild(fill);
  maintainSec.append(maintainHead, maintain);
  box.appendChild(maintainSec);

  // 危险操作：整源清除，单独放在底部并保留一句影响说明
  const danger = el('section', 'md-danger');
  const dangerText = el('div', 'md-danger-text');
  dangerText.append(
    el('div', 'md-danger-title', '清除该媒体源'),
    el('div', 'md-danger-sub', '删除源登记与该源的全部条目记录（含进度、标签关联与搜索索引）。'),
  );
  const clear = el('button', 'btn danger', '清除该源');
  clear.type = 'button';
  clear.disabled = !source.itemCount;
  clear.title = '删除该源在库内的全部条目与源登记，不删除硬盘文件';
  clear.addEventListener('click', () => clearSource(source));
  danger.append(dangerText, clear);
  box.appendChild(danger);
}

/** 重扫一个源：扫描完成后刷新库与当前管理清单。 */
async function rescanSource(source) {
  await scanSource(source);
  await refreshSources();
  await renderManageList();
}

/** 把一个目录合并进指定媒体源（只登记，不移动文件），随后立即扫描。 */
async function mergeDirectory(source) {
  const path = await pickFolder(`选择要合并进「${source.name}」的目录`);
  if (!path) {
    return;
  }
  try {
    await invoke('add_source_root', { id: source.id, path });
    await renderManageList();
    await scanSource(source);
    await refreshSources();
    await renderManageList();
  } catch (err) {
    showToast(String(err));
  }
}

/** 解除一个合并目录：取消登记并清掉该目录下已入库的记录，硬盘文件保留。 */
async function detachRoot(source, path) {
  const ok = await confirmDialog(
    '解除合并目录？',
    `将从「${source.name}」中取消登记 ${path}，并清除该目录下已入库的条目。\n` +
      '硬盘上的视频文件不会被删除。确定继续吗？',
  );
  if (!ok) {
    return;
  }
  try {
    const removed = await invoke('remove_source_root', { id: source.id, path });
    await renderManageList();
    await refreshSources();
    await refresh();
    await reloadFilters();
    showToast(`已解除合并，清除 ${removed} 条记录`);
  } catch (err) {
    showToast(String(err));
  }
}

/** 把用户选中的视频文件登记到指定媒体源（只登记，不移动文件）。 */
async function addVideosToSource(source) {
  const dialog = TAURI && TAURI.dialog;
  if (!dialog || typeof dialog.open !== 'function') {
    showToast('当前环境不支持文件选择');
    return;
  }
  let picked = null;
  try {
    picked = await dialog.open({
      directory: false,
      multiple: true,
      title: `选择要添加到「${source.name}」的视频`,
      filters: [
        {
          name: '视频',
          extensions: ['mp4', 'mkv', 'avi', 'mov', 'wmv', 'flv', 'ts', 'm2ts', 'webm', 'rmvb', 'mpg', 'mpeg', 'm4v'],
        },
      ],
    });
  } catch (err) {
    showToast(String(err));
    return;
  }
  const paths = Array.isArray(picked) ? picked : typeof picked === 'string' ? [picked] : [];
  if (!paths.length) {
    return;
  }
  try {
    const report = await invoke('add_videos_to_source', { id: source.id, paths });
    await renderManageList();
    await refreshSources();
    await refresh();
    await reloadFilters();
    showToast(`已添加 ${report.stored} 条视频`);
  } catch (err) {
    showToast(String(err));
  }
}

/** 整源清除：源登记与条目一起从库中移除，硬盘文件原样保留。 */
async function clearSource(source) {
  const count = source.itemCount || 0;
  const ok = await confirmDialog(
    '清除整个媒体源？',
    `将从库中清除「${source.name}」及它的 ${count} 条记录（含进度、标签关联与搜索索引）。\n` +
      `硬盘上的视频文件不会被删除：${source.location}\n确定继续吗？`,
  );
  if (!ok) {
    return;
  }
  try {
    const removed = await invoke('clear_source', { id: source.id });
    await renderManageList();
    await refresh();
    await reloadFilters();
    showToast(`已清除「${source.name}」的 ${removed} 条记录（视频文件保留在原处）`);
  } catch (err) {
    showToast(String(err));
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
    const merged = source.extraRoots && source.extraRoots.length
      ? ` · 合并 ${source.extraRoots.length} 个目录`
      : '';
    node.textContent = `${source.name}（${source.location}）${merged}${suffix}`;
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
  resetOnlinePanel();
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
  // 「在线字幕」入口：仅当该条目尚无可用字幕轨道时出现在控制栏（单路播放）
  els['pb-sublive'].classList.toggle('hidden', !(playingId && ctx.entryVisible) || liveOn(target));

  const track = ctx.tracks.find((t) => t.id === ctx.reuseTrackId) || null;
  els['sm-state'].textContent = track
    ? `${SUB_KIND_TEXT[track.kind] || '字幕'} · ${typeof track.cueCount === 'number' ? track.cueCount : 0} 句`
    : '未挂载字幕';

  renderSubtitleNotice(target);
  renderSubtitleTracks(target);
  // 菜单高度变化会改变底部让位高度（原生画面不得压住面板）
  syncPlayerPanels();
}

function renderSubtitleNotice(target = subTarget) {
  const ctx = subCtx(target);
  const notes = [];
  if (ctx.info && ctx.info.failure) notes.push(`失败原因：${ctx.info.failure}`);
  if (ctx.degradedNotice) notes.push(ctx.degradedNotice);
  els['sm-notice'].replaceChildren();
  els['sm-notice'].classList.toggle('hidden', notes.length === 0);
  for (const note of notes) els['sm-notice'].appendChild(el('div', 'sm-note', note));
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

// ---- 在线字幕（迅雷影音字幕库：搜索 → 列表 → 点选挂载） ----

/** 搜索序号：用户连续搜索时只采纳最后一次结果。 */
let onlineSearchSeq = 0;

function resetOnlinePanel() {
  els['sm-online-query'].value = '';
  els['sm-online-list'].replaceChildren();
  els['sm-online-list'].classList.add('hidden');
  renderOnlineStatus('');
}

function renderOnlineStatus(text) {
  els['sm-online-status'].textContent = text || '';
  els['sm-online-status'].classList.toggle('hidden', !text);
}

function renderOnlineList(results) {
  els['sm-online-list'].replaceChildren();
  if (!results.length) {
    els['sm-online-list'].classList.add('hidden');
    return;
  }
  els['sm-online-list'].classList.remove('hidden');
  for (const sub of results) {
    const row = el('button', 'sm-online-item');
    row.type = 'button';
    row.title = `${sub.name}（${sub.source || '迅雷字幕库'}）`;
    row.appendChild(el('span', 'so-name', sub.name || '字幕'));
    row.appendChild(el('span', 'so-tag', sub.lang || '未标注'));
    row.appendChild(el('span', 'so-meta', `${(sub.ext || '').toUpperCase()} · ${sub.source || ''}`));
    row.addEventListener('click', () => loadOnlineSubtitle(sub, row));
    els['sm-online-list'].appendChild(row);
  }
}

/** 搜索在线字幕：输入为空时由后端按番号 / 标题 / 文件名自动识别。 */
async function searchOnlineSubtitles() {
  const itemId = subItemId(subTarget);
  if (!itemId) {
    showToast('当前没有正在播放的条目');
    return;
  }
  const query = els['sm-online-query'].value.trim();
  const seq = ++onlineSearchSeq;
  els['sm-online-list'].replaceChildren();
  els['sm-online-list'].classList.add('hidden');
  renderOnlineStatus('正在搜索字幕…');
  let results;
  try {
    results = await invoke('subtitle_online_search', { itemId, query: query || null });
  } catch (err) {
    if (seq === onlineSearchSeq) renderOnlineStatus(`搜索失败：${err}`);
    return;
  }
  if (seq !== onlineSearchSeq) return;
  renderOnlineStatus(results.length
    ? `找到 ${results.length} 条字幕，点选即挂载`
    : '未找到匹配字幕，可换个番号或片名再试');
  renderOnlineList(results);
  syncPlayerPanels();
}

/** 点选一条在线字幕：下载 → 解析 → 保存为外挂轨道 → 立即切换渲染。 */
async function loadOnlineSubtitle(sub, row) {
  const itemId = subItemId(subTarget);
  if (!itemId) {
    showToast('当前没有正在播放的条目');
    return;
  }
  row.disabled = true;
  renderOnlineStatus(`正在下载 ${sub.name}…`);
  try {
    await invoke('subtitle_online_load', {
      itemId,
      url: sub.url,
      lang: sub.lang && sub.lang !== '未标注' ? sub.lang : null,
      slot: subTarget,
    });
  } catch (err) {
    row.disabled = false;
    renderOnlineStatus(`加载失败：${err}`);
    showToast(String(err));
    return;
  }
  await refreshSubtitleTracks(subTarget); // 挂载后切到外挂轨道渲染
  closeSubtitleMenu(); // 收起菜单，让字幕立刻完整显示在画面上
  showToast(`已加载在线字幕：${sub.name}`);
}

/**
 * 单路播放时字幕面板需要的底部让位高度（CSS 像素）。
 *
 * 原生画面窗口恒压在 WebView 之上：字幕菜单 / 弹层若落在画面区域内就会被盖住，
 * 点击还会落到画面上触发暂停，表现为「控件区域卡死」。打开面板时让画面向上收，
 * 由后端把画面宿主缩到控制条 + 让位高度之上。
 */
function panelReserveCss() {
  if (!playingId || inMulti) return 0;
  let need = 0;
  if (!els['subtitle-menu'].classList.contains('hidden')) {
    need = Math.max(need, els['subtitle-menu'].getBoundingClientRect().height + 20);
  }
  for (const id of ['subtitle-privacy', 'subtitle-failure', 'subtitle-external']) {
    if (els[id].classList.contains('hidden')) continue;
    const card = els[id].querySelector('.modal-card');
    need = Math.max(need, (card ? card.getBoundingClientRect().height : 320) + 32);
  }
  // 实时字幕显示期间常驻让出一条：叠层贴在控制条上沿，否则两行字幕同样被画面盖住
  if (liveOn(null)) need = Math.max(need, 96);
  // 播放器设置面板：同样落在画面区域内，需让位
  if (!els['player-settings'].classList.contains('hidden')) {
    need = Math.max(need, els['player-settings'].getBoundingClientRect().height + 20);
  }
  return Math.ceil(need);
}

// ---- 字幕定位：画面内底部（扣掉控制条、面板让位与上下黑边） ----

/** 当前画面的显示宽高比（宽/高）；未知时字幕退回窗口底部。 */
let playerAspect = null;
let aspectTimer = 0;

function resetVideoAspect() {
  playerAspect = null;
  if (aspectTimer) {
    clearTimeout(aspectTimer);
    aspectTimer = 0;
  }
  updateSubtitleStage();
}

/**
 * 轮询读取画面比例：mpv 在解出首帧前读不到 `width/height`，
 * 因此启动后短轮询，成功即停。
 */
function watchVideoAspect() {
  if (aspectTimer) {
    clearTimeout(aspectTimer);
    aspectTimer = 0;
  }
  playerAspect = null;
  let tries = 0;
  const probe = async () => {
    if (!playingId || inMulti) return;
    try {
      const aspect = await invoke('player_video_aspect', { slot: null });
      if (aspect && aspect > 0.05) {
        playerAspect = aspect;
        updateSubtitleStage();
        return;
      }
    } catch (err) {
      /* 属性暂不可读：继续重试，失败不影响播放 */
    }
    tries += 1;
    if (tries < 15 && playingId) aspectTimer = setTimeout(probe, 400);
  };
  probe();
}

/** 把字幕叠层底边放到画面内底部。 */
function updateSubtitleStage() {
  const bar = els['player-bar'];
  const barVisible = bar && !bar.classList.contains('hidden');
  const barH = barVisible && !playerBarHidden ? bar.getBoundingClientRect().height : 0;
  const reserve = playingId && !inMulti ? playerReserveCss : 0;
  const availH = Math.max(0, window.innerHeight - barH - reserve);
  let bottom = barH + reserve;
  if (playerAspect && availH > 0) {
    // mpv 按比例居中：上下黑边各占剩余高度的一半
    const contentH = Math.min(availH, window.innerWidth / playerAspect);
    bottom += Math.max(0, (availH - contentH) / 2);
  }
  document.documentElement.style.setProperty('--sub-bottom', `${Math.round(bottom)}px`);
}

/** 面板开合 / 控制条显隐后同步底部留出量；无变化时不重复下发命令。 */
async function syncPlayerPanels() {
  const need = playingId && !inMulti ? panelReserveCss() : 0;
  playerReserveCss = need;
  updateSubtitleStage();
  // 控制条隐藏且没有面板让位时，画面铺满窗口（负值收掉控制条区域）
  let phys = toPhysical(need);
  if (playerBarHidden && need === 0 && playingId && !inMulti) {
    phys = -constants.controlBarPx;
  }
  if (phys === playerSentReserve) {
    return;
  }
  playerSentReserve = phys;
  try {
    await invoke('set_player_reserve', { extra: phys });
  } catch (err) {
    /* 让位失败不影响面板逻辑，最坏情况仍是画面压住面板 */
  }
}

function openSubtitleMenu() {
  revealPlayerBar();
  els['subtitle-menu'].classList.remove('hidden');
  refreshSubtitleUi(subTarget);
  syncPlayerPanels();
}

function closeSubtitleMenu() {
  els['subtitle-menu'].classList.add('hidden');
  syncPlayerPanels();
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
  syncPlayerPanels();
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
  syncPlayerPanels();
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
  syncPlayerPanels();
}

function hideSubtitleFailure() {
  els['subtitle-failure'].classList.add('hidden');
  syncPlayerPanels();
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
  syncPlayerPanels();
}

function hideExternalModal() {
  els['subtitle-external'].classList.add('hidden');
  syncPlayerPanels();
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
    searchOnlineSubtitles();
  });
  els['sm-close'].addEventListener('click', closeSubtitleMenu);

  els['sm-online-search'].addEventListener('click', searchOnlineSubtitles);
  els['sm-online-query'].addEventListener('keydown', (event) => {
    if (event.key === 'Enter') searchOnlineSubtitles();
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

/** 最多同时选择的路数（3 × 3 = 9）。 */
const MULTI_MAX_SELECT = 9;

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
    // 选择不再按多屏路数拦截：批量清除需要能勾选任意数量，
    // 「进入多屏」由按钮可用性把 4 路上限挡在启动前（见 renderSelectBar）。
    selectedIds.push(item.id);
  }
  renderGrid();
  renderSelectBar();
}

/** 全选当前筛选结果：批量清除时不必逐个点。 */
function selectAllCards() {
  selectedIds = items.filter((item) => !item.missing).map((item) => item.id);
  renderGrid();
  renderSelectBar();
}

/**
 * 批量清除所选条目的入库记录。
 *
 * **只删库内记录，不删硬盘视频文件**（后端 `remove_items` 的边界）；
 * 进度、标签关联、字幕登记随条目级联清除，这里不做二次加工。
 */
async function removeSelected() {
  if (!selectedIds.length) {
    return;
  }
  const count = selectedIds.length;
  if (!(await confirmDialog(
    '清除入库记录',
    `将从库中清除所选 ${count} 部影片的记录（含进度与标签关联）。\n硬盘上的视频文件不会被删除，可随时重新扫描入库。确定继续吗？`,
  ))) {
    return;
  }
  els['select-remove'].disabled = true;
  try {
    const removed = await invoke('remove_items', { ids: selectedIds.slice() });
    exitSelectMode();
    await refresh();
    // 条目消失后，依赖条目存在的派生标签取值也要重算
    await reloadFilters();
    showToast(`已清除 ${removed} 条入库记录（视频文件保留在原处）`);
  } catch (err) {
    showToast(String(err));
  } finally {
    els['select-remove'].disabled = selectedIds.length === 0;
  }
}

function renderSelectBar() {
  els['select-count'].textContent = `已选 ${selectedIds.length} 部`;
  // 进入多屏受格位与解码额度限制（最多 9 路），超限时按钮直接不可用，
  // 而不是勾到第 5 部才弹提示——多选本身要为批量清除放开数量。
  els['select-enter'].disabled = selectedIds.length < 2 || selectedIds.length > MULTI_MAX_SELECT;
  els['select-remove'].disabled = selectedIds.length === 0;
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

/** 上限选项：至少到 9 路（3 × 3 九宫格），已持久化更大的值也照实呈现。 */
function renderLimitOptions(limit) {
  const max = Math.max(9, limit);
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
    const tag = selectedIds.length > 4 ? '3x3' : (selectedIds.length > 2 ? '2x2' : '1x2');
    // 勾选的路数超过当前上限时自动抬高额度（最高 9 路），避免后几路被拒
    let currentLimit = 4;
    try {
      currentLimit = (await invoke('multi_status')).limit || 4;
    } catch (err) {
      currentLimit = 4;
    }
    if (selectedIds.length > currentLimit) {
      await invoke('multi_set_limit', { limit: selectedIds.length });
    }
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
    resetVideoAspect();
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
  large: { cover: 620, wide: 700, grid: 540 },
  mid: { cover: 540, wide: 620, grid: 480 },
  small: { cover: 440, wide: 560, grid: 420 },
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
let consoleAmbientLayer = 0;       // 环境底当前使用的图层（0 / 1 交替做交叉淡化）
let consoleAmbientId = null;       // 环境底正在展示的条目 id（同一影片不重复重绘）
let consoleAmbientTimer = null;    // 旧图层淡出计时
let consoleIdleTimer = null;       // 闲置隐藏顶栏 / 提示条
let consoleWindowMode = false;     // 窗口是否已切到无边框全屏（避免重复调用后端）
let libraryStats = null;           // library_stats 缓存（经典与主机顶栏共用）
/** 推荐看点：后端按高赞短评总结核心看点，随机返回 10 条；「换一批」重新拉取。 */
let consoleRecs = [];
let consoleRecLlm = false;
let consoleRecMessage = '';
let consoleRecLoading = false;
/** 首批推荐是否已拉取过：进入主机模式时不再重复请求，避免刚配好又换一批。 */
let consoleRecLoaded = false;
/** 请求完成后短暂显示「已经完成请求」的时间戳与计时器。 */
let consoleRecDoneUntil = 0;
let consoleRecDoneTimer = null;

/** 主机层是否应当可见：播放中与多屏中一律隐藏，让位后端原生画面窗口。 */
function consoleShown() {
  return consoleOn && !inMulti && !playingId;
}

/**
 * 主机模式窗口形态：进入时切成无边框全屏，退出时还原进入前的窗口位置、尺寸与边框。
 * 状态由后端保存（只保存一份），重复进入不会覆盖最初的位置。
 */
function setConsoleWindowMode(enabled) {
  if (!invoke || consoleWindowMode === enabled) {
    return;
  }
  consoleWindowMode = enabled;
  invoke('set_console_fullscreen', { enabled }).catch((err) => {
    consoleWindowMode = !enabled;
    if (enabled) {
      showToast(`无法切换全屏：${err}`);
    }
  });
}

/** 空闲 4 秒后隐去顶栏与提示条（画面更沉浸）；任何键盘 / 鼠标输入都会唤醒。 */
function wakeConsole() {
  els['console'].classList.remove('idle');
  clearTimeout(consoleIdleTimer);
  consoleIdleTimer = setTimeout(() => {
    if (!consoleShown()) {
      return;
    }
    if (!els['c-settings-panel'].classList.contains('hidden')) {
      return;
    }
    els['console'].classList.add('idle');
  }, 4000);
}

/** 焦点条目：优先全库数据（首页 Hero 可能不在当前筛选结果里）。 */
function consoleItemById(id) {
  if (!id) {
    return null;
  }
  return consoleData.find((i) => i.id === id) || items.find((i) => i.id === id) || null;
}

/**
 * 环境底：把焦点影片的封面大幅虚化后铺满整屏，两层交替交叉淡化。
 * 没有封面时用与占位海报一致的色相渐变兜底，保证任何条目都有背景。
 */
function paintConsoleAmbient(item, force) {
  if (!item || (!force && consoleAmbientId === item.id)) {
    return;
  }
  consoleAmbientId = item.id;
  const layer = consoleAmbientLayer === 0 ? els['c-amb-a'] : els['c-amb-b'];
  const other = consoleAmbientLayer === 0 ? els['c-amb-b'] : els['c-amb-a'];
  const cover = coverCache.get(item.id);
  const hue = hueOf(item.id);
  layer.style.backgroundImage = cover
    ? `linear-gradient(160deg, hsla(${hue}, 55%, 42%, 0.5), hsla(${(hue + 42) % 360}, 52%, 14%, 0.72)), url("${cover}")`
    : `linear-gradient(160deg, hsl(${hue}, 44%, 34%), hsl(${(hue + 42) % 360}, 38%, 11%))`;
  layer.classList.add('on');
  clearTimeout(consoleAmbientTimer);
  consoleAmbientTimer = setTimeout(() => {
    other.classList.remove('on');
  }, 820);
  consoleAmbientLayer = 1 - consoleAmbientLayer;
}

async function enterConsole() {
  if (consoleOn) {
    return;
  }
  if (inMulti) {
    showToast('多屏播放中不能进入主机模式，请先退出多屏');
    return;
  }
  if (playingId) {
    showToast('播放中不能进入主机模式，请先关闭播放页');
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
  consoleAmbientId = null;
  els['c-settings-panel'].classList.add('hidden');
  els['c-preview-toggle'].checked = els['preview-toggle'].checked;
  applyView();
  setConsoleWindowMode(true);
  wakeConsole();
  startConsoleClock();
  await loadConsoleData();
  renderConsole();
  // 推荐看点需要请求大模型：先渲染界面，结果回来后再填这一行；
  // 已拉取过（例如刚在「元数据」里配好大模型）就直接复用，不重复请求。
  if (!consoleRecLoaded) {
    loadConsoleRecommendations();
  }
}

function exitConsole() {
  if (!consoleOn) {
    return;
  }
  consoleOn = false;
  try { localStorage.setItem('cinema.console', '0'); } catch (err) { /* 忽略 */ }
  stopConsoleClock();
  clearTimeout(consoleIdleTimer);
  consoleIdleTimer = null;
  els['console'].classList.remove('idle');
  // 播放中退出时先不还原窗口：等播放页关闭后再还原，避免原生画面窗口尺寸错位。
  if (!playingId) {
    setConsoleWindowMode(false);
  }
  dimsOpen = consoleDimsOpenBackup;
  consoleMissingOnly = false;
  els['c-settings-panel'].classList.add('hidden');
  els['c-search'].blur();
  clearConsoleFocus();
  stopPreview();
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

// ---------------------------------------------------------------------------
// 推荐看点（P-07）：高赞短评 → 大模型总结 → 主机首页横排
// ---------------------------------------------------------------------------

/** 拉取一批看点：每次随机 10 条；已生成过的走缓存，未生成过的才请求大模型。 */
async function loadConsoleRecommendations() {
  if (consoleRecLoading) {
    return;
  }
  consoleRecLoading = true;
  consoleRecDoneUntil = 0;
  clearTimeout(consoleRecDoneTimer);
  updateConsoleRecBand(paintConsoleRecRow());
  let requested = false;
  try {
    const view = await invoke('recommend_highlights', { limit: 10 });
    consoleRecs = Array.isArray(view.items) ? view.items : [];
    consoleRecLlm = !!view.llm;
    consoleRecMessage = view.message || '';
    requested = !!view.requested;
    consoleRecLoaded = true;
  } catch (err) {
    consoleRecs = [];
    consoleRecMessage = String(err);
  } finally {
    consoleRecLoading = false;
  }
  // 真正发过请求时，先显示「已经完成请求」，两秒后回到常规来源标注
  if (requested) {
    consoleRecDoneUntil = Date.now() + 2200;
    clearTimeout(consoleRecDoneTimer);
    consoleRecDoneTimer = setTimeout(() => {
      consoleRecDoneUntil = 0;
      updateConsoleRecBand(paintConsoleRecRow());
      if (consoleOn && consoleScreen === 'home') {
        applyConsoleFocus();
      }
    }, 2200);
  }
  updateConsoleRecBand(paintConsoleRecRow());
  if (consoleOn && consoleScreen === 'home') {
    applyConsoleFocus();
  }
}

/** 只重绘推荐行，不重建整屏：避免刷新时焦点与滚动位置被打乱。 */
function paintConsoleRecRow() {
  const row = els['c-rec-row'];
  if (!row) {
    return [];
  }
  row.replaceChildren();
  let status = '';
  if (consoleRecLoading) {
    status = '正在请求 API，请稍等';
  } else if (Date.now() < consoleRecDoneUntil) {
    status = '已经完成请求';
  } else if (!consoleRecLlm) {
    status = '尚未配置大模型 API · 当前为本地提炼';
  } else if (consoleRecs.length) {
    status = `AI 综合短评与网络资料 · 随机 ${consoleRecs.length} 条`;
  } else {
    status = consoleRecMessage || '暂无可推荐的看点';
  }
  els['c-rec-status'].textContent = status;
  els['c-rec-refresh'].disabled = consoleRecLoading;

  const nodes = [];
  // 未配置大模型时常驻一个入口：点一下直接跳到「元数据 → 推荐看点 · 大模型」
  if (!consoleRecLlm && !consoleRecLoading) {
    const setup = el('button', 'c-rec-setup');
    setup.type = 'button';
    setup.appendChild(el('div', 'c-rec-setup-title', '配置大模型 API'));
    setup.appendChild(
      el('div', 'c-rec-setup-sub', '让看点真正来自 10 条评论与联网资料，而不只是本地提炼。'),
    );
    setup.addEventListener('click', openLlmSettings);
    row.appendChild(setup);
    nodes.push(setup);
  }
  for (const rec of consoleRecs) {
    const card = buildRecCard(rec);
    row.appendChild(card);
    nodes.push(card);
  }
  // 「换一批」放在行尾：键盘一路右移即可到达，Enter 与点击同一条路径
  nodes.push(els['c-rec-refresh']);
  return nodes;
}

function updateConsoleRecBand(nodes) {
  const idx = consoleRows.findIndex((band) => band.kind === 'rec');
  if (idx < 0) {
    return;
  }
  consoleRows[idx] = { kind: 'rec', nodes };
  if (consoleFocus.row === idx) {
    consoleFocus.col = Math.max(0, Math.min(consoleFocus.col, nodes.length - 1));
  }
}

/** 看点卡：正文是总结出的一句核心看点，页脚是影片封面与片名 / 番号 / 赞数。 */
function buildRecCard(rec) {
  const item = consoleItemById(rec.itemId) || { id: rec.itemId, title: rec.title };
  const card = el('div', 'c-rec-card');
  card.dataset.id = rec.itemId;
  card.title = `${rec.title}（单击查看详情）`;

  card.appendChild(el('div', 'c-rec-quote', '“'));
  card.appendChild(el('div', 'c-rec-text', rec.highlight || '暂无看点'));

  const foot = el('div', 'c-rec-foot');
  const cover = el('div', 'c-rec-cover');
  const hue = hueOf(rec.itemId);
  cover.style.background =
    `linear-gradient(158deg, hsl(${hue}, 40%, 27%), hsl(${(hue + 42) % 360}, 34%, 14%))`;
  if (rec.hasPoster) {
    const img = document.createElement('img');
    img.className = 'c-rec-img';
    img.alt = rec.title || '封面';
    img.loading = 'lazy';
    img.decoding = 'async';
    cover.appendChild(img);
    loadCover(rec.itemId, img);
  } else {
    cover.textContent = (rec.title || '影').trim().charAt(0).toUpperCase();
  }
  foot.appendChild(cover);

  const info = el('div', 'c-rec-info');
  info.appendChild(el('div', 'c-rec-name', rec.title || '—'));
  const sub = [];
  if (rec.code) {
    sub.push(rec.code);
  }
  if (rec.rating != null) {
    sub.push(`★ ${Number(rec.rating).toFixed(1)}`);
  }
  if (rec.likes) {
    sub.push(`${rec.likes} 赞`);
  }
  info.appendChild(el('div', 'c-rec-sub', sub.length ? sub.join(' · ') : '暂无番号'));
  // 来源标注：让「用了多少条评论 / 多少项资料」可见，不再像凭空扩写标题
  info.appendChild(
    el(
      'div',
      'c-rec-src',
      `综合 ${rec.comments || 0} 条评论 · ${rec.sources || 0} 项影片资料`,
    ),
  );
  foot.appendChild(info);
  card.appendChild(foot);

  card.addEventListener('mouseenter', () => {
    if (!selectMode && item) {
      scheduleHover(item, card);
    }
  });
  card.addEventListener('mouseleave', () => {
    cancelHover();
    if (consoleOn) {
      cancelConsolePreview();
    }
  });
  card.addEventListener('click', () => {
    if (item) {
      onCardClick(item);
    } else {
      openDetail({ id: rec.itemId, title: rec.title || '' });
    }
  });
  return card;
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
  els['c-rec-row'].replaceChildren();
  els['c-cover-row'].replaceChildren();
  els['c-chips'].replaceChildren();
  els['c-wide-row'].replaceChildren();

  // 顶部不再放「最近观看」大卡：最近观看与继续观看已各有一行，
  // 大卡只是重复同一部影片，白占半屏。首页直接从推荐看点开始。

  // 推荐看点横排：卡片来自后端按高赞短评总结的 10 条核心看点，「换一批」重新随机
  consoleRows.push({ kind: 'rec', nodes: paintConsoleRecRow() });

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
  els['c-wide-row'].parentElement.classList.toggle('hidden', !wideNodes.length);
  if (wideNodes.length) {
    consoleRows.push({ kind: 'wide', nodes: wideNodes });
  }

  // 最近观看封面卡行
  const coverItems = consoleData.slice(0, CONSOLE_COVER_MAX);
  for (const item of coverItems) {
    els['c-cover-row'].appendChild(buildCard(item));
  }
  els['c-cover-row'].parentElement.classList.toggle('hidden', !coverItems.length);
  if (coverItems.length) {
    consoleRows.push({ kind: 'cover', nodes: Array.from(els['c-cover-row'].children) });
  }

  // 分类胶囊行
  const chips = renderConsoleChips(els['c-chips'], true);
  if (chips.length) {
    consoleRows.push({ kind: 'chips', nodes: chips });
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
      el('div', 'empty-sub', '主机模式只提供浏览与播放；请回到经典界面点「添加目录」选择文件夹。'),
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
      node.classList.remove('near');
    }
  }
  els['c-focus-title'].textContent = '';
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

/** 主机模式焦点停留预览：焦点在同一张卡片上停满设定秒数才展开。 */
let consolePreviewTimer = null;

function cancelConsolePreview() {
  if (consolePreviewTimer) {
    clearTimeout(consolePreviewTimer);
    consolePreviewTimer = null;
  }
}

function scheduleConsolePreview(item, node) {
  cancelConsolePreview();
  if (!consoleOn || !item || playingId || inMulti) {
    return;
  }
  if (previewItemId === item.id) {
    return; // 已经在预览这张，不重复起实例
  }
  if (!els['preview-toggle'].checked) {
    return;
  }
  consolePreviewTimer = setTimeout(() => {
    consolePreviewTimer = null;
    startPreview(item, node);
  }, previewDelayMs());
}

/** 鼠标移到主机模式卡片上时，把焦点同步到这张卡（键盘继续从该位置移动）。 */
function focusConsoleNode(node) {
  if (!consoleOn || !node) {
    return;
  }
  for (let r = 0; r < consoleRows.length; r += 1) {
    const col = consoleRows[r].nodes.indexOf(node);
    if (col >= 0) {
      if (consoleFocus.row === r && consoleFocus.col === col) {
        return;
      }
      consoleFocus = { row: r, col, desiredCol: col };
      applyConsoleFocus();
      return;
    }
  }
}

/** 统一出口：清旧焦点 → 夹紧行列 → 给当前节点加焦点 → 滚入可视区 → 同步环境底与提示条。 */
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
  // PS5 式选择：相邻卡片轻微后退，突出当前卡
  const at = band.nodes.indexOf(node);
  band.nodes.forEach((peer, i) => {
    peer.classList.toggle('near', i !== at && Math.abs(i - at) <= 2);
  });
  scrollConsoleTo(node);
  // 沉浸式环境底与底部标题跟随焦点影片（分类胶囊等无条目的节点保持现状）
  const item = consoleItemById(node.dataset ? node.dataset.id : '');
  if (item) {
    paintConsoleAmbient(item);
    els['c-focus-title'].textContent = item.title || '';
  } else {
    els['c-focus-title'].textContent = '';
  }
  // 无论鼠标悬停还是主机焦点，停留够设定秒数即展开放大预览
  scheduleConsolePreview(item, node);
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
    stopPreview();
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
  wakeConsole(); // 任何按键都让顶栏 / 提示条重新出现
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
    // WSAD 与方向键同义：主机模式用键盘选片
    case 'a':
    case 'A':
    case 'ArrowLeft':
      event.preventDefault();
      moveConsoleFocus(0, -1);
      break;
    case 'd':
    case 'D':
    case 'ArrowRight':
      event.preventDefault();
      moveConsoleFocus(0, 1);
      break;
    case 'w':
    case 'W':
    case 'ArrowUp':
      event.preventDefault();
      moveConsoleFocus(-1, 0);
      break;
    case 's':
    case 'S':
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
  // 批量 / 自动刮削进度：整批结束时刷新封面与标签（封面文件可能刚被替换）
  await listen('scrape-progress', (event) => {
    const payload = event && event.payload ? event.payload : {};
    updateScrapeProgress(payload);
    if (payload.total && payload.done >= payload.total) {
      coverCache.clear();
      refresh();
      reloadFilters();
    }
  });
  // 画面窗口截获的点击：预览 → 打开详情（与直接点海报一致）；正式播放 → 切换暂停
  await listen('preview-click', () => {
    const id = previewItemId;
    // 详情页里播的是预告片：点画面只收起预览，不跳播放页
    if (!els['detail-modal'].classList.contains('hidden')) {
      stopPreview();
      return;
    }
    // 主机模式下画面点击只收起预览（详情弹层属于经典界面）
    if (consoleOn) {
      stopPreview();
      return;
    }
    const item = items.find((i) => i.id === id);
    stopPreview();
    if (item) {
      openDetail(item);
    }
  });
  await listen('player-click', () => {
    if (!playingId) return;
    // 控制条已收起时，点击画面只负责唤醒控制条，不误触暂停
    if (playerBarHidden) {
      revealPlayerBar();
      return;
    }
    togglePause();
  });
  await listen('player-move', () => {
    if (!playingId) return;
    // 刚收起控制条时画面窗口刚被撑开，这段窗口内的移动事件多为假事件
    if (Date.now() < playerBarWakeBlockUntil) return;
    revealPlayerBar();
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

  els['add-btn'].addEventListener('click', addSource);
  els['scan-cancel'].addEventListener('click', cancelScan);
  // 库管理：左栏选源、右栏操作（只删库内记录，不删硬盘文件）
  els['manage-btn'].addEventListener('click', openManageModal);
  els['manage-close'].addEventListener('click', hideManageModal);
  els['manage-close-2'].addEventListener('click', hideManageModal);
  // 点遮罩关闭；点面板内部不关
  els['manage-modal'].addEventListener('click', (event) => {
    if (event.target === els['manage-modal']) {
      hideManageModal();
    }
  });
  // 元数据数据源：站点地址 + 登录 Cookie；保存即验证
  els['meta-btn'].addEventListener('click', openMetaModal);
  els['meta-save'].addEventListener('click', saveMetaSettings);
  // 服务商切换：预置服务商自动带出接口地址与模型下拉
  els['llm-provider'].addEventListener('change', () => {
    applyLlmProvider(els['llm-provider'].value, '');
  });
  els['meta-scrape-all'].addEventListener('click', () => scrapeMissing(null));
  els['meta-close'].addEventListener('click', () => els['meta-modal'].classList.add('hidden'));
  // 条目详情：单击海报进入；播放正片 / 预告片、从数据源更新
  els['detail-play'].addEventListener('click', playFromDetail);
  els['detail-trailer'].addEventListener('click', playDetailTrailer);
  els['detail-refresh'].addEventListener('click', scrapeCurrentItem);
  els['detail-x'].addEventListener('click', hideDetail);
  // 点击详情页以外的遮罩区域也关闭（点内容区不关）
  els['detail-modal'].addEventListener('click', (event) => {
    if (event.target === els['detail-modal']) {
      hideDetail();
    }
  });
  els['tag-back'].addEventListener('click', backFromTagBrowse);
  els['lightbox'].addEventListener('click', closeLightbox);
  // 预告片画面窗口已设为鼠标穿透，点击落到全屏遮罩上即关闭；
  // 画面下方的进度条属于操作区，自身的点击不能冒泡触发关闭
  els['trailer-stage'].addEventListener('click', stopPreview);
  els['trailer-controls'].addEventListener('click', (event) => event.stopPropagation());
  els['trailer-seek'].addEventListener('input', () => {
    trailerSeeking = true;
  });
  els['trailer-seek'].addEventListener('change', async () => {
    trailerSeeking = false;
    if (!trailerDurationMs) return;
    const positionMs = Math.round((Number(els['trailer-seek'].value) / 1000) * trailerDurationMs);
    try {
      await invoke('trailer_seek', { positionMs });
    } catch (err) {
      showToast(String(err));
    }
  });
  els['trailer-close'].addEventListener('click', (event) => {
    event.stopPropagation();
    stopPreview();
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
  // 放大预览舞台的控制条（位于原生画面窗口之外，可正常点击）
  els['preview-sound'].addEventListener('click', togglePreviewSound);
  els['preview-close'].addEventListener('click', stopPreview);

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
  // 播放器设置：快进时长 / 预览停留 / 预览声音
  els['pb-settings'].addEventListener('click', () => {
    if (els['player-settings'].classList.contains('hidden')) openPlayerSettings();
    else closePlayerSettings();
  });
  els['pset-close'].addEventListener('click', closePlayerSettings);
  els['pset-save'].addEventListener('click', savePlayerSettings);
  for (const id of ['pset-forward', 'pset-preview-delay']) {
    els[id].addEventListener('keydown', (event) => {
      if (event.key === 'Enter') savePlayerSettings();
    });
  }
  document.addEventListener('click', (event) => {
    if (els['player-settings'].classList.contains('hidden')) return;
    if (event.target.closest('#player-settings') || event.target.closest('#pb-settings')) return;
    closePlayerSettings();
  });
  // 播放中按右方向键快进（经典 / 主机模式播放页通用），左键保持系统默认
  document.addEventListener('keydown', (event) => {
    if (event.key !== 'ArrowRight' || !playingId || inMulti) return;
    if (isTypingTarget(event.target) || modalOpen()) return;
    event.preventDefault();
    revealPlayerBar();
    forwardBy(forwardSeconds);
  });
  // 鼠标在页面层移动：唤醒自动隐藏的控制条（原生画面上的移动由后端转发）
  let lastRevealAt = 0;
  document.addEventListener('mousemove', () => {
    if (!playingId) return;
    if (Date.now() < playerBarWakeBlockUntil) return;
    const now = Date.now();
    if (!playerBarHidden && now - lastRevealAt < 800) return;
    lastRevealAt = now;
    revealPlayerBar();
  });
  els['pb-seek'].addEventListener('input', () => { seeking = true; });
  els['pb-seek'].addEventListener('change', async () => {
    seeking = false;
    if (!currentDurationMs) {
      return;
    }
    const positionMs = Math.round((Number(els['pb-seek'].value) / 1000) * currentDurationMs);
    lastPositionMs = positionMs;
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
  els['select-all'].addEventListener('click', selectAllCards);
  els['select-enter'].addEventListener('click', enterMulti);
  els['select-remove'].addEventListener('click', removeSelected);
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
  els['console'].addEventListener('mousemove', wakeConsole);
  els['c-search-btn'].addEventListener('click', openConsoleSearch);
  els['c-to-classic'].addEventListener('click', exitConsole);
  els['c-to-classic-2'].addEventListener('click', exitConsole);
  els['c-settings'].addEventListener('click', () => {
    els['c-settings-panel'].classList.toggle('hidden');
  });
  els['c-settings-close'].addEventListener('click', () => {
    els['c-settings-panel'].classList.add('hidden');
  });
  // 推荐看点：换一批重新随机 10 条；尚未生成过看点的条目会请求大模型
  els['c-rec-refresh'].addEventListener('click', () => {
    loadConsoleRecommendations();
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

  // 预览正在播放时，一旦指针离开其所占矩形就停止。
  // 详情页的预告片是手动点播：指针在按钮上（不在画面矩形内），不能因为动一下就停，
  // 只由「点画面 / 关闭详情 / 5 分钟兜底」结束。
  window.addEventListener('mousemove', (event) => {
    if (els['detail-modal'].classList.contains('hidden') && previewRect && pointerOutsidePreview(event)) {
      stopPreview();
    }
  });
  window.addEventListener('blur', stopPreview);
  window.addEventListener('resize', () => {
    stopPreview();
    // 窗口尺寸变化后格位矩形随之改变，重新摆放原生画面窗口
    if (inMulti && !browsingLibrary) placeAllSlots();
    // 字幕叠层依赖可用画面高度与黑边，尺寸变化后重新定位
    updateSubtitleStage();
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
