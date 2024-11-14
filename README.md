# openai-realtime-demo

This is a quick&dirty hack to consume OpenAI Realtime API.

Requirements:
- Any recent Linux distro
- Recent rust, see [rustup](https://rustup.rs)
- SSL cert/key
- OpenAI api key

Instructions:

```
cargo build
export CRT="www.crt"
export KEY="www.key"
export PORT="8080"
export OPENAI_API_KEY="your_open_api_key"
export OPENAI_INSTRUCTIONS="Your knowledge cutoff is 2023-10. You are a helpful assistant."
cargo run
```

Then point your browser at https://hostname...:8080 . Click on "Hello" and start speaking.
