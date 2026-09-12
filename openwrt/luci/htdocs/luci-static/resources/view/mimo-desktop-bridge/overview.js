'use strict';
'require view';
'require uci';
'require fs';

return view.extend({
	load: function () {
		return uci.load('mimo_desktop_bridge');
	},

	render: function () {
		var port = uci.get('mimo_desktop_bridge', 'main', 'port') || '8787';
		var host = location.hostname || '192.168.1.1';
		var url = 'http://' + host + ':' + port + '/';

		var frame = E('iframe', {
			src: url,
			style: 'width:100%;height:70vh;border:1px solid #ccc;border-radius:8px;background:#0f1115'
		});

		return E([], [
			E('h2', {}, _('MiMo Desktop Bridge')),
			E('p', { class: 'cbi-map-descr' },
				_('Free-channel OpenAI/Anthropic bridge. Sign in with a Xiaomi account in the embedded WebUI.')),
			E('p', {}, [
				_('Base URL') + ': ',
				E('a', { href: url, target: '_blank' }, url)
			]),
			frame
		]);
	},

	handleSave: null,
	handleSaveApply: null,
	handleReset: null
});
