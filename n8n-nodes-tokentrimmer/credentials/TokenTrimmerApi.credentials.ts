import type {
	IAuthenticateGeneric,
	ICredentialTestRequest,
	ICredentialType,
	INodeProperties,
} from 'n8n-workflow';

export class TokenTrimmerApi implements ICredentialType {
	name = 'tokenTrimmerApi';

	displayName = 'TokenTrimmer API';

	documentationUrl =
		'https://github.com/tokentrimmer/tokentrimmer/blob/main/n8n-nodes-tokentrimmer/README.md';

	properties: INodeProperties[] = [
		{
			displayName: 'API Key',
			name: 'apiKey',
			type: 'string',
			typeOptions: { password: true },
			default: '',
			description:
				'Hosted: a TokenTrimmer key (tt_live_* / tt_test_*). Self-hosted pass-through: your provider API key, forwarded upstream.',
		},
		{
			displayName: 'Base URL',
			name: 'baseUrl',
			type: 'string',
			default: 'https://api.tokentrimmer.com',
			description:
				'Gateway origin WITHOUT /v1 (the node appends it). Self-host: http://localhost:8080 — from a containerized n8n use http://host.docker.internal:8080.',
		},
	];

	authenticate: IAuthenticateGeneric = {
		type: 'generic',
		properties: {
			headers: {
				Authorization: '=Bearer {{$credentials.apiKey}}',
			},
		},
	};

	// GET /v1/models is a cheap authenticated probe (API ref §5.1).
	// The baseURL expression mirrors the node's normalizeBaseUrl forgiveness
	// (trim, strip trailing slashes, strip ONE trailing /v1) so a pasted
	// SDK-style base URL passes the credential test exactly when the node
	// itself would work. Kept in lockstep by test/credential.test.ts.
	test: ICredentialTestRequest = {
		request: {
			baseURL:
				'={{ $credentials.baseUrl.trim().replace(/\\/+$/, "").replace(/\\/v1$/i, "").replace(/\\/+$/, "") }}',
			url: '/v1/models',
		},
	};
}
