const path = require("path");
const Dotenv = require("dotenv-webpack");

module.exports = {
	entry: "./index.ts",
	output: {
		filename: "index.js",
		path: path.resolve(__dirname, "dist"),
	},
	resolve: {
		extensions: [".ts", ".js"],
	},
	module: {
		rules: [
			{
				test: /\.ts$/,
				use: "ts-loader",
				exclude: /node_modules/,
			},
		],
	},
	plugins: [
		new Dotenv(),
		// new webpack.IgnorePlugin({
		// 	resourceRegExp: /^utf-8-validate$|^bufferutil$/,
		// }),
	],
	target: "node",
	mode: "production",
	externals: {
		"utf-8-validate": "commonjs utf-8-validate",
		"bufferutil": "commonjs bufferutil",
	}
};
