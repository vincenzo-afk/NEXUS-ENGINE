(function () {
  'use strict';

  /* ========================================================================
     State
     ======================================================================== */
  const state = {
    query: '',
    results: [],
    totalResults: 0,
    page: 1,
    pageSize: 10,
    hasMore: false,
    loading: false,
    suggestions: [],
    activeIndex: -1,
    selectedIndex: -1,
    autocompleteOpen: false,
    filtersOpen: false,
    shortcutsOpen: false,
    debounceTimer: null,
    abortController: null,
    filters: {
      filetype: '',
      site: '',
      date: '',
    },
    history: [],
    saved: [],
    historyOpen: false,
    savedOpen: false,
  };

  /* ========================================================================
     DOM refs
     ======================================================================== */
  const $ = (id) => document.getElementById(id);
  const $$ = (sel, ctx) => (ctx || document).querySelectorAll(sel);
  const el = {
    input: $('search-input'),
    clear: $('search-clear'),
    autocomplete: $('autocomplete-list'),
    results: $('results-list'),
    resultsSection: $('results-section'),
    resultsStats: $('results-stats'),
    pagination: $('pagination'),
    prevPage: $('prev-page'),
    nextPage: $('next-page'),
    pageInfo: $('page-info'),
    moreSection: $('more-section'),
    showMore: $('show-more-btn'),
    loadAll: $('load-all-btn'),
    errorSection: $('error-section'),
    errorMessage: $('error-message'),
    retryBtn: $('retry-btn'),
    emptySection: $('empty-section'),
    emptyMessage: $('empty-message'),
    themeToggle: $('theme-toggle'),
    filterToggle: $('filter-toggle'),
    filterPanel: $('filter-panel'),
    filterFiletype: $('filter-filetype'),
    filterSite: $('filter-site'),
    filterDate: $('filter-date'),
    filterApply: $('filter-apply'),
    filterReset: $('filter-reset'),
    shortcutsHelp: $('keyboard-shortcuts-help'),
    shortcutsClose: $$('.shortcuts-close')[0],
    hero: $$('.hero')[0],
    placeholder: $('results-placeholder'),
    historyToggle: $('history-toggle'),
    historyPanel: $('history-panel'),
    historyList: $('history-list'),
    historyEmpty: $('history-empty'),
    historyClear: $('history-clear'),
    savedToggle: $('saved-toggle'),
    savedPanel: $('saved-panel'),
    savedList: $('saved-list'),
    savedEmpty: $('saved-empty'),
    savedAdd: $('saved-add'),
    exportJsonBtn: $('export-json-btn'),
    exportCsvBtn: $('export-csv-btn'),
  };

  /* ========================================================================
     Theme
     ======================================================================== */
  function getPreferredTheme() {
    const stored = localStorage.getItem('nexus-theme');
    if (stored === 'light' || stored === 'dark') return stored;
    return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  }

  function setTheme(theme) {
    document.documentElement.setAttribute('data-theme', theme);
    localStorage.setItem('nexus-theme', theme);
  }

  function toggleTheme() {
    const current = document.documentElement.getAttribute('data-theme');
    setTheme(current === 'dark' ? 'light' : 'dark');
  }

  setTheme(getPreferredTheme());

  /* ========================================================================
     URL hash routing
     ======================================================================== */
  function parseHash() {
    const hash = window.location.hash.slice(1);
    if (!hash) return {};
    try {
      const params = new URLSearchParams(hash);
      const q = params.get('q') || '';
      const page = parseInt(params.get('p'), 10) || 1;
      return { q, page };
    } catch {
      return {};
    }
  }

  function updateHash(q, page) {
    const params = new URLSearchParams();
    if (q) params.set('q', q);
    if (page > 1) params.set('p', page);
    const hash = params.toString();
    const currentHash = window.location.hash.slice(1);
    if (hash !== currentHash) {
      window.location.hash = hash;
    }
  }

  function onHashChange() {
    const { q, page } = parseHash();
    if (q && q !== state.query) {
      state.query = q;
      state.page = page || 1;
      el.input.value = q;
      doSearch(true);
    } else if (!q) {
      resetToHome();
    }
  }

  window.addEventListener('hashchange', onHashChange);

  /* ========================================================================
     Search
     ======================================================================== */
  function buildSearchUrl(query, page, filters) {
    const params = new URLSearchParams();
    params.set('q', query);
    if (page > 1) params.set('p', page);
    if (state.pageSize !== 10) params.set('n', state.pageSize);
    if (filters.filetype) params.set('filetype', filters.filetype);
    if (filters.site) params.set('site', filters.site);
    if (filters.date) params.set('date', filters.date);
    return `/search?${params.toString()}`;
  }

  function buildSuggestUrl(prefix) {
    const params = new URLSearchParams();
    params.set('prefix', prefix);
    return `/suggest?${params.toString()}`;
  }

  function doSearch(resetPage) {
    if (resetPage) {
      state.page = 1;
      state.results = [];
    }
    state.query = el.input.value.trim();
    if (!state.query) {
      resetToHome();
      return;
    }

    if (state.abortController) {
      state.abortController.abort();
    }
    state.abortController = new AbortController();

    state.loading = true;
    state.selectedIndex = -1;
    state.activeIndex = -1;
    closeAutocomplete();
    hideError();
    hideEmpty();
    showLoading();

    const url = buildSearchUrl(state.query, state.page, state.filters);
    updateHash(state.query, state.page);

    fetch(url, {
      signal: state.abortController.signal,
      headers: { 'Accept': 'application/json' },
    })
      .then(function (res) {
        if (!res.ok) throw new Error('Search request failed');
        return res.json();
      })
      .then(function (data) {
        state.totalResults = data.total || 0;
        state.hasMore = data.hasMore || false;

        const newResults = data.results || [];
        if (resetPage || state.page === 1) {
          state.results = newResults;
        } else {
          state.results = state.results.concat(newResults);
        }

        renderResults();
        state.loading = false;
        state.abortController = null;
        if (resetPage) {
          recordHistory(state.query);
        }
      })
      .catch(function (err) {
        if (err.name === 'AbortError') return;
        state.loading = false;
        state.abortController = null;
        showError(err.message || 'An error occurred while searching.');
      });
  }

  /* ========================================================================
     Autocomplete
     ======================================================================== */
  function fetchSuggestions(prefix) {
    if (!prefix || prefix.length < 2) {
      closeAutocomplete();
      return;
    }

    if (state.suggestAbort) {
      state.suggestAbort.abort();
    }
    state.suggestAbort = new AbortController();

    fetch(buildSuggestUrl(prefix), {
      signal: state.suggestAbort.signal,
      headers: { 'Accept': 'application/json' },
    })
      .then(function (res) {
        if (!res.ok) return [];
        return res.json();
      })
      .then(function (data) {
        if (state.suggestAbort && state.suggestAbort.signal.aborted) return;
        state.suggestions = (data.suggestions || data || []);
        renderAutocomplete();
      })
      .catch(function () {});
  }

  function renderAutocomplete() {
    const list = el.autocomplete;
    list.innerHTML = '';
    if (!state.suggestions.length) {
      closeAutocomplete();
      return;
    }

    state.suggestions.forEach(function (s, i) {
      const li = document.createElement('li');
      li.className = 'autocomplete-item';
      li.setAttribute('role', 'option');
      li.setAttribute('aria-selected', 'false');
      li.dataset.index = i;

      const iconSpan = document.createElement('span');
      iconSpan.className = 'autocomplete-item-icon';
      iconSpan.innerHTML =
        '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/></svg>';

      const textSpan = document.createElement('span');
      textSpan.className = 'autocomplete-item-text';
      textSpan.textContent = s;

      li.appendChild(iconSpan);
      li.appendChild(textSpan);

      li.addEventListener('mousedown', function (e) {
        e.preventDefault();
        selectSuggestion(i);
      });

      list.appendChild(li);
    });

    el.input.setAttribute('aria-expanded', 'true');
    list.hidden = false;
    state.autocompleteOpen = true;
  }

  function closeAutocomplete() {
    el.autocomplete.hidden = true;
    el.autocomplete.innerHTML = '';
    el.input.setAttribute('aria-expanded', 'false');
    el.input.setAttribute('aria-activedescendant', '');
    state.autocompleteOpen = false;
    state.suggestions = [];
  }

  function selectSuggestion(index) {
    const suggestion = state.suggestions[index];
    if (!suggestion) return;
    el.input.value = suggestion;
    closeAutocomplete();
    doSearch(true);
  }

  /* ========================================================================
     Autocomplete keyboard navigation
     ======================================================================== */
  function navigateAutocomplete(direction) {
    const items = $$('.autocomplete-item', el.autocomplete);
    if (!items.length) return;

    let idx = state.activeIndex;
    items.forEach(function (item) {
      item.classList.remove('active');
      item.setAttribute('aria-selected', 'false');
    });

    if (direction === 'down') {
      idx = idx < items.length - 1 ? idx + 1 : 0;
    } else {
      idx = idx > 0 ? idx - 1 : items.length - 1;
    }

    state.activeIndex = idx;
    items[idx].classList.add('active');
    items[idx].setAttribute('aria-selected', 'true');
    items[idx].scrollIntoView({ block: 'nearest' });

    el.input.setAttribute('aria-activedescendant', '');
    el.input.setAttribute('aria-expanded', 'true');
  }

  function openSuggestion() {
    if (state.activeIndex >= 0 && state.suggestions[state.activeIndex]) {
      selectSuggestion(state.activeIndex);
      return true;
    }
    return false;
  }

  /* ========================================================================
     Highlight matches
     ======================================================================== */
  function highlightText(text, query) {
    if (!text || !query) return escapeHtml(text);
    const escaped = escapeHtml(text);
    const words = query.trim().split(/\s+/).filter(Boolean);
    let result = escaped;
    words.forEach(function (word) {
      const safe = escapeHtml(word).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      if (!safe) return;
      const regex = new RegExp('(' + safe + ')', 'gi');
      result = result.replace(regex, '<mark>$1</mark>');
    });
    return result;
  }

  function escapeHtml(str) {
    if (!str) return '';
    var div = document.createElement('div');
    div.appendChild(document.createTextNode(str));
    return div.innerHTML;
  }

  /* ========================================================================
     Render results
     ======================================================================== */
  function renderResults() {
    hideLoading();
    hideEmpty();
    hideError();

    if (!state.results.length) {
      showEmpty('No results found for "' + state.query + '". Try different keywords or remove filters.');
      el.resultsSection.hidden = false;
      el.pagination.hidden = true;
      el.moreSection.hidden = true;
      el.loadAll.hidden = true;
      el.exportJsonBtn.hidden = true;
      el.exportCsvBtn.hidden = true;
      el.resultsStats.textContent = '';
      document.body.classList.add('has-results');
      return;
    }

    el.resultsSection.hidden = false;
    document.body.classList.add('has-results');

    const list = el.results;
    list.innerHTML = '';
    list.setAttribute('role', 'list');

    const startingRank = (state.page - 1) * state.pageSize + 1;

    state.results.forEach(function (result, i) {
      const card = document.createElement('article');
      card.className = 'result-card';
      card.setAttribute('role', 'listitem');
      card.dataset.index = i;

      const urlDiv = document.createElement('div');
      urlDiv.className = 'result-url';
      urlDiv.innerHTML =
        '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/></svg>' +
        escapeHtml(result.url || '');

      const titleDiv = document.createElement('div');
      titleDiv.className = 'result-title';
      titleDiv.innerHTML = highlightText(result.title || result.url, state.query);

      const snippetDiv = document.createElement('div');
      snippetDiv.className = 'result-snippet';
      snippetDiv.innerHTML = highlightText(result.snippet || '', state.query);

      const metaDiv = document.createElement('div');
      metaDiv.className = 'result-meta';
      const parts = [];
      if (result.size) parts.push(escapeHtml(result.size));
      if (result.date) parts.push(escapeHtml(result.date));
      if (result.filetype) parts.push(escapeHtml(result.filetype.toUpperCase()));
      metaDiv.textContent = parts.join(' \u00B7 ');

      card.appendChild(urlDiv);
      card.appendChild(titleDiv);
      card.appendChild(snippetDiv);
      if (parts.length) card.appendChild(metaDiv);

      card.addEventListener('click', function () {
        openResult(i);
      });

      list.appendChild(card);
    });

    const total = state.totalResults || state.results.length;
    const from = startingRank;
    const to = startingRank + state.results.length - 1;
    el.resultsStats.textContent = 'About ' + total.toLocaleString() + ' results (' + from + '\u2013' + to + ').';

    if (state.hasMore && state.results.length >= state.pageSize) {
      el.moreSection.hidden = false;
      el.loadAll.hidden = false;
    } else {
      el.moreSection.hidden = true;
      el.loadAll.hidden = true;
    }
    el.exportJsonBtn.hidden = false;
    el.exportCsvBtn.hidden = false;
    el.pagination.hidden = true;
    updatePagination();

    focusInput();
  }

  /* ========================================================================
     Open result
     ======================================================================== */
  function openResult(index, newTab) {
    const result = state.results[index];
    if (!result || !result.url) return;
    if (newTab) {
      window.open(result.url, '_blank', 'noopener,noreferrer');
    } else {
      window.location.href = result.url;
    }
  }

  /* ========================================================================
     Pagination
     ======================================================================== */
  function updatePagination() {
    const totalPages = Math.max(1, Math.ceil((state.totalResults || state.results.length) / state.pageSize));
    el.pageInfo.textContent = 'Page ' + state.page + ' of ' + totalPages;
    el.prevPage.disabled = state.page <= 1;
    el.nextPage.disabled = state.page >= totalPages;

    if (state.results.length > state.pageSize || totalPages > 1) {
      el.pagination.hidden = false;
    }
  }

  function goToPage(delta) {
    const totalPages = Math.max(1, Math.ceil((state.totalResults || state.results.length) / state.pageSize));
    const newPage = Math.max(1, Math.min(totalPages, state.page + delta));
    if (newPage === state.page) return;
    state.page = newPage;
    doSearch(true);
  }

  function showMore() {
    state.page += 1;
    doSearch(false);
  }

  function loadAllAtOnce() {
    state.pageSize = 50;
    state.page = 1;
    doSearch(true);
  }

  /* ========================================================================
     Filters
     ======================================================================== */
  function toggleFilters() {
    state.filtersOpen = !state.filtersOpen;
    el.filterPanel.hidden = !state.filtersOpen;
    el.filterToggle.setAttribute('aria-expanded', String(state.filtersOpen));
  }

  function applyFilters() {
    state.filters.filetype = el.filterFiletype.value;
    state.filters.site = el.filterSite.value.trim();
    state.filters.date = el.filterDate.value;
    doSearch(true);
  }

  function resetFilters() {
    el.filterFiletype.value = '';
    el.filterSite.value = '';
    el.filterDate.value = '';
    state.filters.filetype = '';
    state.filters.site = '';
    state.filters.date = '';
    doSearch(true);
  }

  /* ========================================================================
     Search history (localStorage, client-side only)
     ======================================================================== */
  var HISTORY_KEY = 'nexus_search_history';
  var SAVED_KEY = 'nexus_saved_searches';
  var HISTORY_MAX = 50;

  function loadHistory() {
    try {
      var raw = window.localStorage.getItem(HISTORY_KEY);
      state.history = raw ? JSON.parse(raw) : [];
    } catch (e) {
      state.history = [];
    }
  }

  function persistHistory() {
    try {
      window.localStorage.setItem(HISTORY_KEY, JSON.stringify(state.history));
    } catch (e) {
      // Storage may be unavailable (private browsing, quota exceeded, etc.);
      // history just won't persist across reloads in that case.
    }
  }

  function recordHistory(query) {
    if (!query) return;
    state.history = state.history.filter(function (h) { return h.query !== query; });
    state.history.unshift({ query: query, ts: Date.now() });
    if (state.history.length > HISTORY_MAX) {
      state.history = state.history.slice(0, HISTORY_MAX);
    }
    persistHistory();
    if (state.historyOpen) renderHistory();
  }

  function removeHistoryEntry(index) {
    state.history.splice(index, 1);
    persistHistory();
    renderHistory();
  }

  function clearHistory() {
    state.history = [];
    persistHistory();
    renderHistory();
  }

  function renderHistory() {
    el.historyList.innerHTML = '';
    el.historyEmpty.hidden = state.history.length > 0;

    state.history.forEach(function (entry, i) {
      var li = document.createElement('li');
      li.className = 'dropdown-list-item';

      var text = document.createElement('span');
      text.className = 'dropdown-list-item-text';
      text.textContent = entry.query;
      text.addEventListener('click', function () {
        el.input.value = entry.query;
        state.query = entry.query;
        closeHistoryPanel();
        doSearch(true);
      });

      var remove = document.createElement('button');
      remove.className = 'dropdown-list-item-remove';
      remove.setAttribute('aria-label', 'Remove from history');
      remove.textContent = '\u00D7';
      remove.addEventListener('click', function (e) {
        e.stopPropagation();
        removeHistoryEntry(i);
      });

      li.appendChild(text);
      li.appendChild(remove);
      el.historyList.appendChild(li);
    });
  }

  function toggleHistoryPanel() {
    closeSavedPanel();
    state.historyOpen = !state.historyOpen;
    el.historyPanel.hidden = !state.historyOpen;
    el.historyToggle.setAttribute('aria-expanded', String(state.historyOpen));
    if (state.historyOpen) renderHistory();
  }

  function closeHistoryPanel() {
    state.historyOpen = false;
    el.historyPanel.hidden = true;
    el.historyToggle.setAttribute('aria-expanded', 'false');
  }

  /* ========================================================================
     Saved searches (localStorage, client-side only)
     ======================================================================== */
  function loadSaved() {
    try {
      var raw = window.localStorage.getItem(SAVED_KEY);
      state.saved = raw ? JSON.parse(raw) : [];
    } catch (e) {
      state.saved = [];
    }
  }

  function persistSaved() {
    try {
      window.localStorage.setItem(SAVED_KEY, JSON.stringify(state.saved));
    } catch (e) {
      // See persistHistory() — storage may be unavailable.
    }
  }

  function addSavedSearch() {
    var query = el.input.value.trim();
    if (!query) {
      window.alert('Type a search first, then click "Save current".');
      return;
    }
    var name = window.prompt('Name this saved search:', query);
    if (name === null) return; // user cancelled
    name = name.trim() || query;

    state.saved = state.saved.filter(function (s) { return s.name !== name; });
    state.saved.unshift({
      name: name,
      query: query,
      filters: {
        filetype: state.filters.filetype,
        site: state.filters.site,
        date: state.filters.date,
      },
      ts: Date.now(),
    });
    persistSaved();
    renderSaved();
  }

  function loadSavedSearch(entry) {
    el.input.value = entry.query;
    state.query = entry.query;
    if (entry.filters) {
      state.filters.filetype = entry.filters.filetype || '';
      state.filters.site = entry.filters.site || '';
      state.filters.date = entry.filters.date || '';
      if (el.filterFiletype) el.filterFiletype.value = state.filters.filetype;
      if (el.filterSite) el.filterSite.value = state.filters.site;
      if (el.filterDate) el.filterDate.value = state.filters.date;
    }
    closeSavedPanel();
    doSearch(true);
  }

  function removeSavedSearch(index) {
    state.saved.splice(index, 1);
    persistSaved();
    renderSaved();
  }

  function renderSaved() {
    el.savedList.innerHTML = '';
    el.savedEmpty.hidden = state.saved.length > 0;

    state.saved.forEach(function (entry, i) {
      var li = document.createElement('li');
      li.className = 'dropdown-list-item';

      var text = document.createElement('span');
      text.className = 'dropdown-list-item-text';
      text.textContent = entry.name;
      text.title = entry.query;
      text.addEventListener('click', function () {
        loadSavedSearch(entry);
      });

      var remove = document.createElement('button');
      remove.className = 'dropdown-list-item-remove';
      remove.setAttribute('aria-label', 'Remove saved search');
      remove.textContent = '\u00D7';
      remove.addEventListener('click', function (e) {
        e.stopPropagation();
        removeSavedSearch(i);
      });

      li.appendChild(text);
      li.appendChild(remove);
      el.savedList.appendChild(li);
    });
  }

  function toggleSavedPanel() {
    closeHistoryPanel();
    state.savedOpen = !state.savedOpen;
    el.savedPanel.hidden = !state.savedOpen;
    el.savedToggle.setAttribute('aria-expanded', String(state.savedOpen));
    if (state.savedOpen) renderSaved();
  }

  function closeSavedPanel() {
    state.savedOpen = false;
    el.savedPanel.hidden = true;
    el.savedToggle.setAttribute('aria-expanded', 'false');
  }

  /* ========================================================================
     Export results (client-side only; exports whatever is currently
     loaded in state.results, i.e. what the user has actually seen)
     ======================================================================== */
  function triggerDownload(filename, content, mimeType) {
    var blob = new Blob([content], { type: mimeType });
    var url = URL.createObjectURL(blob);
    var a = document.createElement('a');
    a.href = url;
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    setTimeout(function () { URL.revokeObjectURL(url); }, 1000);
  }

  function exportResultsAsJson() {
    if (!state.results.length) return;
    var payload = {
      query: state.query,
      exportedAt: new Date().toISOString(),
      count: state.results.length,
      results: state.results,
    };
    triggerDownload(
      'nexus-results-' + slugify(state.query) + '.json',
      JSON.stringify(payload, null, 2),
      'application/json'
    );
  }

  function csvEscape(value) {
    var str = value === undefined || value === null ? '' : String(value);
    if (/[",\n]/.test(str)) {
      str = '"' + str.replace(/"/g, '""') + '"';
    }
    return str;
  }

  function exportResultsAsCsv() {
    if (!state.results.length) return;
    var header = ['rank', 'title', 'url', 'snippet', 'size', 'date', 'filetype', 'score'];
    var rows = [header.join(',')];
    state.results.forEach(function (r, i) {
      rows.push([
        i + 1,
        csvEscape(r.title || ''),
        csvEscape(r.url || ''),
        csvEscape(r.snippet || ''),
        csvEscape(r.size || ''),
        csvEscape(r.date || ''),
        csvEscape(r.filetype || ''),
        csvEscape(r.score !== undefined ? r.score : ''),
      ].join(','));
    });
    triggerDownload(
      'nexus-results-' + slugify(state.query) + '.csv',
      rows.join('\r\n'),
      'text/csv'
    );
  }

  function slugify(str) {
    return (str || 'export')
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, '-')
      .replace(/^-+|-+$/g, '')
      .slice(0, 40) || 'export';
  }

  /* ========================================================================
     Loading / Error / Empty states
     ======================================================================== */
  function showLoading() {
    hideError();
    hideEmpty();
    const placeholder = el.placeholder;
    if (placeholder) {
      placeholder.innerHTML =
        '<div class="spinner" role="status"><span class="sr-only">Loading...</span></div>';
      placeholder.hidden = false;
    }
    el.resultsSection.hidden = false;
    el.results.innerHTML = '';
    el.results.appendChild(el.placeholder);
    el.pagination.hidden = true;
    el.moreSection.hidden = true;
    el.loadAll.hidden = true;
    el.resultsStats.textContent = 'Searching\u2026';
  }

  function hideLoading() {
    const placeholder = el.placeholder;
    if (placeholder) {
      placeholder.hidden = true;
    }
  }

  function showError(msg) {
    el.errorMessage.textContent = msg || 'Something went wrong. Please try again.';
    el.errorSection.hidden = false;
    el.resultsSection.hidden = true;
    el.pagination.hidden = true;
    el.moreSection.hidden = true;
    el.loadAll.hidden = true;
  }

  function hideError() {
    el.errorSection.hidden = true;
  }

  function showEmpty(msg) {
    el.emptyMessage.textContent = msg || 'No results found.';
    el.emptySection.hidden = false;
    el.pagination.hidden = true;
    el.moreSection.hidden = true;
    el.loadAll.hidden = true;
  }

  function hideEmpty() {
    el.emptySection.hidden = true;
  }

  function resetToHome() {
    state.query = '';
    state.results = [];
    state.totalResults = 0;
    state.page = 1;
    state.hasMore = false;
    state.loading = false;
    state.selectedIndex = -1;
    state.activeIndex = -1;
    document.body.classList.remove('has-results');
    el.resultsSection.hidden = true;
    el.errorSection.hidden = true;
    el.emptySection.hidden = true;
    el.pagination.hidden = true;
    el.moreSection.hidden = true;
    el.loadAll.hidden = true;
    el.exportJsonBtn.hidden = true;
    el.exportCsvBtn.hidden = true;
    el.results.innerHTML = '';
    el.resultsStats.textContent = '';
    if (window.location.hash) {
      window.location.hash = '';
    }
  }

  /* ========================================================================
     UI helpers
     ======================================================================== */
  function focusInput() {
    if (document.activeElement !== el.input) {
      el.input.focus();
    }
  }

  function clearSearch() {
    el.input.value = '';
    state.query = '';
    closeAutocomplete();
    resetToHome();
    el.input.focus();
  }

  function showClearButton() {
    el.clear.hidden = !el.input.value.length;
  }

  /* ========================================================================
     Keyboard navigation for results
     ======================================================================== */
  function navigateResults(direction) {
    if (!state.results.length) return;
    const cards = $$('.result-card');
    if (!cards.length) return;

    let idx = state.selectedIndex;
    cards.forEach(function (c) { c.classList.remove('active'); });

    if (direction === 'down') {
      idx = idx < cards.length - 1 ? idx + 1 : 0;
    } else {
      idx = idx > 0 ? idx - 1 : cards.length - 1;
    }

    state.selectedIndex = idx;
    cards[idx].classList.add('active');
    cards[idx].scrollIntoView({ block: 'nearest' });

    el.input.setAttribute('aria-activedescendant', '');
  }

  function openSelectedResult(newTab) {
    if (state.selectedIndex >= 0 && state.results[state.selectedIndex]) {
      openResult(state.selectedIndex, newTab);
      return true;
    }
    return false;
  }

  /* ========================================================================
     Event handlers
     ======================================================================== */
  function onSearchInput() {
    const val = el.input.value;
    state.query = val;
    showClearButton();

    if (state.debounceTimer) {
      clearTimeout(state.debounceTimer);
    }

    if (val.trim().length >= 2) {
      fetchSuggestions(val.trim());

      state.debounceTimer = setTimeout(function () {
        if (val.trim()) {
          doSearch(true);
        } else {
          resetToHome();
        }
      }, 300);
    } else {
      closeAutocomplete();
      if (!val) {
        resetToHome();
      }
    }
  }

  function onSearchKeydown(e) {
    if (e.key === 'Enter') {
      e.preventDefault();
      if (state.autocompleteOpen) {
        if (openSuggestion()) return;
      }
      closeAutocomplete();
      doSearch(true);
    } else if (e.key === 'Escape') {
      if (state.autocompleteOpen) {
        closeAutocomplete();
      } else if (el.input.value) {
        clearSearch();
      }
    } else if (e.key === 'ArrowDown') {
      if (state.autocompleteOpen) {
        e.preventDefault();
        navigateAutocomplete('down');
      } else {
        e.preventDefault();
        navigateResults('down');
      }
    } else if (e.key === 'ArrowUp') {
      if (state.autocompleteOpen) {
        e.preventDefault();
        navigateAutocomplete('up');
      } else {
        e.preventDefault();
        navigateResults('up');
      }
    }
  }

  function onGlobalKeydown(e) {
    const tag = document.activeElement && document.activeElement.tagName;

    if (e.key === '/' && tag !== 'INPUT' && tag !== 'TEXTAREA') {
      e.preventDefault();
      el.input.focus();
      el.input.select();
      return;
    }

    if (e.key === '?' && !e.ctrlKey && !e.metaKey) {
      e.preventDefault();
      toggleShortcutsHelp();
      return;
    }

    if (e.key === 'Escape') {
      if (state.shortcutsOpen) {
        closeShortcutsHelp();
        return;
      }
      if (state.historyOpen) {
        closeHistoryPanel();
        return;
      }
      if (state.savedOpen) {
        closeSavedPanel();
        return;
      }
    }

    if (e.ctrlKey && e.shiftKey && (e.key === 'D' || e.key === 'd')) {
      e.preventDefault();
      toggleTheme();
      return;
    }

    if (e.ctrlKey && e.key === 'Enter') {
      if (document.activeElement === el.input && state.selectedIndex >= 0) {
        e.preventDefault();
        openSelectedResult(true);
      }
    }
  }

  /* ========================================================================
     Keyboard shortcuts help modal
     ======================================================================== */
  function toggleShortcutsHelp() {
    state.shortcutsOpen = !state.shortcutsOpen;
    el.shortcutsHelp.hidden = !state.shortcutsOpen;
    if (state.shortcutsOpen) {
      const btn = el.shortcutsHelp.querySelector('.shortcuts-close');
      if (btn) btn.focus();
    }
  }

  function closeShortcutsHelp() {
    state.shortcutsOpen = false;
    el.shortcutsHelp.hidden = true;
    el.input.focus();
  }

  /* ========================================================================
     Bootstrap
     ======================================================================== */
  function init() {
    showClearButton();
    loadHistory();
    loadSaved();

    el.input.addEventListener('input', onSearchInput);
    el.input.addEventListener('keydown', onSearchKeydown);
    document.addEventListener('keydown', onGlobalKeydown);

    el.clear.addEventListener('click', clearSearch);
    el.themeToggle.addEventListener('click', toggleTheme);
    el.filterToggle.addEventListener('click', toggleFilters);
    el.filterApply.addEventListener('click', applyFilters);
    el.filterReset.addEventListener('click', resetFilters);

    el.historyToggle.addEventListener('click', toggleHistoryPanel);
    el.historyClear.addEventListener('click', clearHistory);
    el.savedToggle.addEventListener('click', toggleSavedPanel);
    el.savedAdd.addEventListener('click', addSavedSearch);
    el.exportJsonBtn.addEventListener('click', exportResultsAsJson);
    el.exportCsvBtn.addEventListener('click', exportResultsAsCsv);

    document.addEventListener('click', function (e) {
      if (state.historyOpen && !el.historyPanel.contains(e.target) && e.target !== el.historyToggle && !el.historyToggle.contains(e.target)) {
        closeHistoryPanel();
      }
      if (state.savedOpen && !el.savedPanel.contains(e.target) && e.target !== el.savedToggle && !el.savedToggle.contains(e.target)) {
        closeSavedPanel();
      }
    });

    el.prevPage.addEventListener('click', function () { goToPage(-1); });
    el.nextPage.addEventListener('click', function () { goToPage(1); });
    el.showMore.addEventListener('click', showMore);
    el.loadAll.addEventListener('click', loadAllAtOnce);
    el.retryBtn.addEventListener('click', function () { doSearch(true); });

    el.shortcutsClose.addEventListener('click', closeShortcutsHelp);
    el.shortcutsHelp.addEventListener('click', function (e) {
      if (e.target.classList.contains('shortcuts-overlay')) {
        closeShortcutsHelp();
      }
    });

    el.shortcutsHelp.addEventListener('keydown', function (e) {
      if (e.key === 'Escape') {
        closeShortcutsHelp();
      }
    });

    {
      const { q, page } = parseHash();
      if (q) {
        state.query = q;
        state.page = page || 1;
        el.input.value = q;
        doSearch(true);
      }
    }

    el.input.focus();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }

})();
