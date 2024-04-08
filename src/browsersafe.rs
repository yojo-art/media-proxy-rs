pub const FILE_TYPE_BROWSERSAFE: [&str; 20] = [
	// OggS
	"audio/opus",
	"video/ogg",
	"audio/ogg",
	"application/ogg",

	// ISO/IEC base media file format
	"video/quicktime",
	"video/mp4",
	"audio/mp4",
	"video/x-m4v",
	"audio/x-m4a",
	"video/3gpp",
	"video/3gpp2",

	"video/mpeg",
	"audio/mpeg",

	"video/webm",
	"audio/webm",

	"audio/aac",

	// see https://github.com/misskey-dev/misskey/pull/10686
	"audio/flac",
	"audio/wav",
	// backward compatibility
	"audio/x-flac",
	"audio/vnd.wave",
];
