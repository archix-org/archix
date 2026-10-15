export const archixTheme = {
	name: 'archix',
	type: 'dark',
	colors: { 'editor.background': '#0f1b2d', 'editor.foreground': '#d8e4ef' },
	tokenColors: [
		{ scope: ['comment', 'punctuation.definition.comment'], settings: { foreground: '#6f8299', fontStyle: 'italic' } },
		{ scope: ['keyword', 'storage', 'keyword.control'], settings: { foreground: '#7ebae4' } },
		{ scope: ['string', 'constant.numeric', 'constant.language'], settings: { foreground: '#e6d3a3' } },
		{ scope: ['entity.name.function', 'support.function', 'meta.function-call', 'variable.other.property'], settings: { foreground: '#4fb3e8' } },
		{ scope: ['entity.name.tag', 'support.type.property-name', 'variable.other.key'], settings: { foreground: '#b9dcf2' } },
		{ scope: ['punctuation', 'meta.brace'], settings: { foreground: '#8fa2b8' } },
	],
} as const;
