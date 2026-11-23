// The theme, chosen or the system's, set before the page is first drawn. The application, a
// module, runs only once its files have all come, and a dark page would show light until then.
// app.js keeps it as it changes.
{
  const chosen = localStorage.getItem('epiphany.theme') ?? 'system';
  const dark = chosen === 'dark' || (chosen === 'system' && matchMedia('(prefers-color-scheme: dark)').matches);
  document.documentElement.dataset.theme = dark ? 'dark' : 'light';
}
