# Use cloud-only dual-provider transcription for the first product

Voisu will stream each Recording to Groq and Deepgram concurrently and will not
run or train a local speech model in the first product. This preserves the
user's limited workstation capacity for normal work while retaining the
quality and fallback benefits of independent cloud providers.

