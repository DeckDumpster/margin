// Runs from <head>: the wrap choice is applied before first paint.
(function () {
  var root = document.documentElement;
  try {
    if (localStorage.getItem('margin.wrap') === 'off') root.classList.add('nowrap');
  } catch (e) {}
  document.addEventListener('DOMContentLoaded', function () {
    var button = document.getElementById('wrap-toggle');
    if (!button) return;
    function sync() {
      var off = root.classList.contains('nowrap');
      button.textContent = off ? 'Wrap: off' : 'Wrap: on';
      button.setAttribute('aria-pressed', off ? 'false' : 'true');
    }
    sync();
    button.addEventListener('click', function () {
      var off = root.classList.toggle('nowrap');
      try {
        localStorage.setItem('margin.wrap', off ? 'off' : 'on');
      } catch (e) {}
      sync();
    });
  });
})();
