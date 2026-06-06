function switchInstallTab(tab) {
  var curlBlock = document.getElementById('install-curl');
  var brewBlock = document.getElementById('install-brew');
  if (!curlBlock || !brewBlock) return;

  var tabs = document.querySelectorAll('.install-tab');
  tabs.forEach(function(t) {
    if (t.dataset.tab === tab) {
      t.classList.add('install-tab-active');
    } else {
      t.classList.remove('install-tab-active');
    }
  });

  if (tab === 'curl') {
    curlBlock.classList.remove('hidden');
    brewBlock.classList.add('hidden');
  } else {
    curlBlock.classList.add('hidden');
    brewBlock.classList.remove('hidden');
  }
}

function copyCommand(cmd, btn) {
  function showCopied() {
    btn.innerHTML = '<svg class="w-5 h-5 text-green-400" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7"/></svg><span class="copy-tooltip">Copied!</span>';
    setTimeout(function() {
      btn.innerHTML = '<svg class="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 16H6a2 2 0 01-2-2V6a2 2 0 012-2h8a2 2 0 012 2v2m-6 12h8a2 2 0 002-2v-8a2 2 0 00-2-2h-8a2 2 0 00-2 2v8a2 2 0 002 2z"/></svg>';
    }, 2000);
  }

  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(cmd).then(showCopied).catch(function() {
      fallbackCopy(cmd);
      showCopied();
    });
  } else {
    fallbackCopy(cmd);
    showCopied();
  }
}

function fallbackCopy(text) {
  var textarea = document.createElement('textarea');
  textarea.value = text;
  textarea.style.position = 'fixed';
  textarea.style.opacity = '0';
  document.body.appendChild(textarea);
  textarea.select();
  document.execCommand('copy');
  document.body.removeChild(textarea);
}

// Mobile sidebar toggle
function toggleMobileSidebar(btn) {
  var expanded = btn.getAttribute('aria-expanded') === 'true';
  var menuId = btn.getAttribute('aria-controls');
  var menu = document.getElementById(menuId);
  if (!menu) return;

  btn.setAttribute('aria-expanded', String(!expanded));
  menu.classList.toggle('hidden');
}

// Fetch GitHub star count (only when the badge is on the page; it isn't on docs pages)
const starCountEl = document.getElementById('star-count');
if (starCountEl) {
  fetch('https://api.github.com/repos/agent-of-empires/agent-of-empires')
    .then(res => {
      if (!res.ok) throw new Error('star count fetch failed: ' + res.status);
      return res.json();
    })
    .then(data => {
      const count = data.stargazers_count;
      if (count !== undefined) {
        starCountEl.textContent = count >= 1000 ? (count / 1000).toFixed(1) + 'k' : count;
      }
    })
    .catch(() => {
      starCountEl.textContent = '';
    });
}

// Theme toggle
function initThemeToggle() {
  function updateIcons(theme) {
    document.querySelectorAll('.theme-icon-sun').forEach(function(el) {
      el.classList.toggle('hidden', theme === 'light');
    });
    document.querySelectorAll('.theme-icon-moon').forEach(function(el) {
      el.classList.toggle('hidden', theme === 'dark');
    });
    document.querySelectorAll('.theme-label').forEach(function(el) {
      el.textContent = theme === 'dark' ? 'Light mode' : 'Dark mode';
    });
  }

  var currentTheme = document.documentElement.dataset.theme || 'dark';
  updateIcons(currentTheme);

  document.querySelectorAll('#theme-toggle, #theme-toggle-mobile').forEach(function(btn) {
    btn.addEventListener('click', function() {
      var next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
      document.documentElement.dataset.theme = next;
      document.documentElement.style.colorScheme = next;
      localStorage.setItem('theme', next);
      updateIcons(next);
    });
  });
}

initThemeToggle();

// Scroll-triggered animations
document.addEventListener('DOMContentLoaded', () => {
  const observer = new IntersectionObserver((entries) => {
    entries.forEach((entry) => {
      if (entry.isIntersecting) {
        entry.target.classList.add('is-visible');
        observer.unobserve(entry.target);
      }
    });
  }, { threshold: 0.1 });

  document.querySelectorAll('.animate-on-scroll').forEach((el) => {
    observer.observe(el);
  });
});
