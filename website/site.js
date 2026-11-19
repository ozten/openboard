const copy = document.querySelector('#copy');
copy.addEventListener('click', async () => {
  const status = document.querySelector('#copy-status');
  try {
    await navigator.clipboard.writeText(document.querySelector('#command').textContent);
    status.textContent = 'Copied. Paste into your terminal to install.';
    copy.textContent = 'Copied!';
  } catch {
    status.textContent = 'Select and copy the command above. Clipboard access is unavailable in this browser.';
  }
});
