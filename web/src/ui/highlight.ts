// Dependency-free, best-effort syntax highlighting (ADR-0026) for the two small
// languages the editors author: the rules language (epiphany-calc) and flow
// TypeScript. It is a colorizer, not a parser: it tokenizes loosely and is only
// kept roughly in step with the Rust lexers. Output is HTML with the source
// fully escaped; a quirk can only mis-color, never change the saved text.

export type CodeLanguage = 'rules' | 'flow'

function escapeHtml(s: string): string {
  return s.replace(/[&<>]/g, (c) => (c === '&' ? '&amp;' : c === '<' ? '&lt;' : '&gt;'))
}

/** A scan rule: a sticky regex and the token class to emit (or a classifier, or
 * `null` for un-styled text). Tried in order at each position; first hit wins. */
interface Rule {
  re: RegExp
  cls: string | null | ((m: string) => string | null)
}

const RULES_KEYWORDS = new Set([
  'if', 'then', 'else', 'and', 'or', 'not', 'value', 'with', 'leaves',
  'consolidated', 'all', 'children', 'descendants', 'of',
])

const FLOW_KEYWORDS = new Set([
  'function', 'const', 'let', 'var', 'return', 'if', 'else', 'for', 'while',
  'do', 'break', 'continue', 'new', 'typeof', 'instanceof', 'in', 'of', 'void',
  'delete', 'true', 'false', 'null', 'undefined', 'this', 'throw', 'try',
  'catch', 'finally', 'switch', 'case', 'default',
])

function identClass(keywords: Set<string>): (m: string) => string | null {
  return (m) => (keywords.has(m) ? 'kw' : null)
}

function rulesRules(): Rule[] {
  return [
    { re: /\/\*[\s\S]*?\*\//y, cls: 'comment' },
    { re: /'(?:[^'\\]|\\.)*'/y, cls: 'str' },
    { re: /\b\d+(?:\.\d+)?\b/y, cls: 'num' },
    { re: /[A-Za-z_][A-Za-z0-9_]*/y, cls: identClass(RULES_KEYWORDS) },
    { re: /[[\]]/y, cls: 'bracket' },
    { re: /[-+*/=<>!,:;@(){}]/y, cls: 'punct' },
  ]
}

function flowRules(): Rule[] {
  return [
    { re: /\/\/[^\n]*/y, cls: 'comment' },
    { re: /\/\*[\s\S]*?\*\//y, cls: 'comment' },
    { re: /'(?:[^'\\]|\\.)*'|"(?:[^"\\]|\\.)*"|`(?:[^`\\]|\\.)*`/y, cls: 'str' },
    { re: /\b\d+(?:\.\d+)?\b/y, cls: 'num' },
    { re: /[A-Za-z_$][A-Za-z0-9_$]*/y, cls: identClass(FLOW_KEYWORDS) },
    { re: /[-+*/=<>!&|?,:;.(){}[\]]/y, cls: 'punct' },
  ]
}

function tokenize(src: string, rules: Rule[]): string {
  let i = 0
  let out = ''
  while (i < src.length) {
    let matched = false
    for (const rule of rules) {
      rule.re.lastIndex = i
      const m = rule.re.exec(src)
      if (m && m[0].length > 0) {
        const text = escapeHtml(m[0])
        const cls = typeof rule.cls === 'function' ? rule.cls(m[0]) : rule.cls
        out += cls ? `<span class="tok-${cls}">${text}</span>` : text
        i += m[0].length
        matched = true
        break
      }
    }
    if (!matched) {
      out += escapeHtml(src[i])
      i += 1
    }
  }
  return out
}

/** Highlight `src` for `language`, returning escaped HTML with token spans. */
export function highlight(src: string, language: CodeLanguage): string {
  return tokenize(src, language === 'rules' ? rulesRules() : flowRules())
}
