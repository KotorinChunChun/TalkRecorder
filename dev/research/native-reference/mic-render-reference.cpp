#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <mmreg.h>
#include <wrl/client.h>
#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <deque>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <string>
#include <thread>
#include <vector>

using Microsoft::WRL::ComPtr;
using Steady = std::chrono::steady_clock;

static UINT64 qpc100ns() {
    LARGE_INTEGER ticks{}, frequency{};
    if (!QueryPerformanceCounter(&ticks) || !QueryPerformanceFrequency(&frequency) ||
        ticks.QuadPart < 0 || frequency.QuadPart <= 0) return 0;
    const UINT64 t = static_cast<UINT64>(ticks.QuadPart), f = static_cast<UINT64>(frequency.QuadPart);
    return t / f * 10000000ULL + (t % f) * 10000000ULL / f;
}

template<class Function>
__declspec(noinline) static HRESULT optional_api(Function* function) {
    __try { return (*function)(); }
    __except (EXCEPTION_EXECUTE_HANDLER) {
        return static_cast<HRESULT>(GetExceptionCode() | FACILITY_NT_BIT);
    }
}

template<class Function>
static HRESULT guarded_clock(Function function) { return optional_api(&function); }

struct ComGuard {
    HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    ~ComGuard() { if (SUCCEEDED(hr)) CoUninitialize(); }
};

struct FormatGuard {
    WAVEFORMATEX* value = nullptr;
    ~FormatGuard() { CoTaskMemFree(value); }
};

struct RunningGuard {
    IAudioClient* capture = nullptr;
    IAudioClient* render = nullptr;
    ~RunningGuard() {
        if (capture) capture->Stop();
        if (render) render->Stop();
    }
};

struct SourcePacket {
    UINT64 device_frame = 0, native_qpc = 0, received_qpc = 0;
    UINT32 total_frames = 0, consumed = 0;
    DWORD flags = 0;
    std::vector<BYTE> data;
};

struct ClockSample {
    UINT64 position = 0, qpc = 0, frequency = 0;
    HRESULT position_hr = E_NOINTERFACE, frequency_hr = E_NOINTERFACE;
    bool valid = false;
};

struct Mapping {
    const char* event = "copy";
    HRESULT operation_hr = S_OK;
    UINT64 source_device_frame = 0, source_native_qpc = 0, source_received_qpc = 0;
    UINT32 source_packet_frames = 0, source_offset_frames = 0;
    DWORD source_flags = 0;
    UINT64 renderer_written_frame = 0;
    UINT32 frames_written = 0, render_padding = 0, queued_source_frames = 0;
    UINT64 copy_before_qpc = 0, copy_after_qpc = 0;
    ClockSample clock;
    UINT64 source_start_estimated_qpc = 0, render_present_estimated_qpc = 0;
    INT64 expected_delay_100ns = 0;
    bool expected_valid = false;
};

static bool is_float32(const WAVEFORMATEX* format) {
    if (!format || format->wBitsPerSample != 32 ||
        format->nBlockAlign != format->nChannels * sizeof(float)) return false;
    if (format->wFormatTag == WAVE_FORMAT_IEEE_FLOAT) return true;
    if (format->wFormatTag != WAVE_FORMAT_EXTENSIBLE || format->cbSize < 22) return false;
    const auto* extended = reinterpret_cast<const WAVEFORMATEXTENSIBLE*>(format);
    static const GUID float_subtype{3, 0, 0x0010, {0x80,0,0,0xaa,0,0x38,0x9b,0x71}};
    return IsEqualGUID(extended->SubFormat, float_subtype) && extended->Samples.wValidBitsPerSample == 32;
}

static bool same_format(const WAVEFORMATEX* a, const WAVEFORMATEX* b) {
    if (!a || !b || a->wFormatTag != b->wFormatTag || a->nChannels != b->nChannels ||
        a->nSamplesPerSec != b->nSamplesPerSec || a->nAvgBytesPerSec != b->nAvgBytesPerSec ||
        a->nBlockAlign != b->nBlockAlign || a->wBitsPerSample != b->wBitsPerSample || a->cbSize != b->cbSize) return false;
    return a->cbSize == 0 || std::memcmp(reinterpret_cast<const BYTE*>(a) + sizeof(WAVEFORMATEX),
                                        reinterpret_cast<const BYTE*>(b) + sizeof(WAVEFORMATEX), a->cbSize) == 0;
}

static bool run_reference(const std::filesystem::path& folder, const std::filesystem::path& stop_file,
                          int seconds) {
    std::ofstream csv(folder / "mapping.csv", std::ios::out | std::ios::trunc);
    if (!csv) { std::cerr << "Cannot open mapping.csv\n"; return false; }
    csv << "event,elapsed_ms,operation_hr,sample_rate,channels,source_device_frame,source_native_qpc_100ns,"
        "source_received_qpc_100ns,source_packet_frames,source_offset_frames,source_flags,"
        "renderer_written_frame,frames_written,render_padding,queued_source_frames,copy_before_qpc_100ns,"
        "copy_after_qpc_100ns,render_clock_position,render_clock_qpc_100ns,render_clock_frequency,"
        "render_clock_position_hr,render_clock_frequency_hr,render_clock_valid,source_start_estimated_qpc_100ns,"
        "render_present_estimated_qpc_100ns,expected_delay_100ns,expected_valid,capture_latency_100ns,"
        "render_latency_100ns,capture_latency_hr,render_latency_hr\n";
    csv.flush();
    const auto created = Steady::now();
    UINT32 rate = 0;
    WORD channels = 0;
    REFERENCE_TIME capture_latency = 0, render_latency = 0;
    HRESULT capture_latency_hr = E_FAIL, render_latency_hr = E_FAIL;
    auto write = [&](const Mapping& row) {
        const auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(Steady::now() - created).count();
        csv << row.event << ',' << elapsed << ',' << static_cast<UINT32>(row.operation_hr) << ',' << rate << ',' << channels
            << ',' << row.source_device_frame << ',' << row.source_native_qpc << ',' << row.source_received_qpc
            << ',' << row.source_packet_frames << ',' << row.source_offset_frames << ',' << row.source_flags
            << ',' << row.renderer_written_frame << ',' << row.frames_written << ',' << row.render_padding
            << ',' << row.queued_source_frames << ',' << row.copy_before_qpc << ',' << row.copy_after_qpc
            << ',' << row.clock.position << ',' << row.clock.qpc << ',' << row.clock.frequency
            << ',' << static_cast<UINT32>(row.clock.position_hr) << ',' << static_cast<UINT32>(row.clock.frequency_hr)
            << ',' << static_cast<int>(row.clock.valid) << ',' << row.source_start_estimated_qpc
            << ',' << row.render_present_estimated_qpc << ',' << row.expected_delay_100ns
            << ',' << static_cast<int>(row.expected_valid) << ',' << capture_latency << ',' << render_latency
            << ',' << static_cast<UINT32>(capture_latency_hr) << ',' << static_cast<UINT32>(render_latency_hr) << '\n';
    };
    auto fail = [&](const char* operation, HRESULT hr) {
        Mapping row{}; row.event = operation; row.operation_hr = hr; row.copy_before_qpc = qpc100ns();
        write(row); csv.flush();
        std::cerr << operation << " hr=" << static_cast<UINT32>(hr) << std::endl;
        return false;
    };
    ComGuard com;
    if (FAILED(com.hr)) return fail("error_CoInitializeEx", com.hr);
    ComPtr<IMMDeviceEnumerator> enumerator;
    HRESULT hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL, IID_PPV_ARGS(&enumerator));
    if (FAILED(hr) || !enumerator) return fail("error_CreateEnumerator", FAILED(hr) ? hr : E_POINTER);
    ComPtr<IMMDevice> capture_endpoint, render_endpoint;
    hr = enumerator->GetDefaultAudioEndpoint(eCapture, eConsole, &capture_endpoint);
    if (FAILED(hr) || !capture_endpoint) return fail("error_CaptureEndpoint", FAILED(hr) ? hr : E_POINTER);
    hr = enumerator->GetDefaultAudioEndpoint(eRender, eConsole, &render_endpoint);
    if (FAILED(hr) || !render_endpoint) return fail("error_RenderEndpoint", FAILED(hr) ? hr : E_POINTER);
    ComPtr<IAudioClient> capture_client, render_client;
    hr = capture_endpoint->Activate(__uuidof(IAudioClient), CLSCTX_ALL, nullptr,
                                   reinterpret_cast<void**>(capture_client.GetAddressOf()));
    if (FAILED(hr) || !capture_client) return fail("error_ActivateCapture", FAILED(hr) ? hr : E_POINTER);
    hr = render_endpoint->Activate(__uuidof(IAudioClient), CLSCTX_ALL, nullptr,
                                  reinterpret_cast<void**>(render_client.GetAddressOf()));
    if (FAILED(hr) || !render_client) return fail("error_ActivateRender", FAILED(hr) ? hr : E_POINTER);
    FormatGuard capture_format, render_format;
    hr = capture_client->GetMixFormat(&capture_format.value);
    if (FAILED(hr)) return fail("error_CaptureFormat", hr);
    hr = render_client->GetMixFormat(&render_format.value);
    if (FAILED(hr)) return fail("error_RenderFormat", hr);
    if (!same_format(capture_format.value, render_format.value) || !is_float32(capture_format.value))
        return fail("error_FormatsMustMatchFloat32", AUDCLNT_E_UNSUPPORTED_FORMAT);
    rate = capture_format.value->nSamplesPerSec;
    channels = capture_format.value->nChannels;
    const UINT32 block_align = capture_format.value->nBlockAlign;
    if (rate == 0 || rate > 384000 || channels == 0 || channels > 32) return fail("error_InvalidFormat", E_INVALIDARG);
    hr = capture_client->Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1000000, 0, capture_format.value, nullptr);
    if (FAILED(hr)) return fail("error_InitializeCapture", hr);
    hr = render_client->Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1000000, 0, render_format.value, nullptr);
    if (FAILED(hr)) return fail("error_InitializeRender", hr);
    ComPtr<IAudioCaptureClient> capture;
    ComPtr<IAudioRenderClient> render;
    hr = capture_client->GetService(IID_PPV_ARGS(&capture));
    if (FAILED(hr) || !capture) return fail("error_CaptureService", FAILED(hr) ? hr : E_POINTER);
    hr = render_client->GetService(IID_PPV_ARGS(&render));
    if (FAILED(hr) || !render) return fail("error_RenderService", FAILED(hr) ? hr : E_POINTER);
    ComPtr<IAudioClock> render_clock;
    const HRESULT clock_service_hr = guarded_clock([&] { return render_client->GetService(IID_PPV_ARGS(&render_clock)); });
    UINT64 clock_frequency = 0;
    const HRESULT clock_frequency_hr = render_clock ? guarded_clock([&] { return render_clock->GetFrequency(&clock_frequency); }) : clock_service_hr;
    capture_latency_hr = capture_client->GetStreamLatency(&capture_latency);
    render_latency_hr = render_client->GetStreamLatency(&render_latency);
    UINT32 render_buffer_frames = 0;
    hr = render_client->GetBufferSize(&render_buffer_frames);
    if (FAILED(hr) || render_buffer_frames == 0) return fail("error_RenderBufferSize", FAILED(hr) ? hr : E_FAIL);
    const UINT32 target_padding = std::min(render_buffer_frames, std::max<UINT32>(1, rate / 20));
    bool clock_disabled = false;
    HRESULT disabled_clock_hr = E_NOINTERFACE;
    auto read_clock = [&]() {
        ClockSample sample{};
        sample.frequency = clock_frequency; sample.frequency_hr = clock_frequency_hr;
        sample.position_hr = clock_disabled ? disabled_clock_hr : (render_clock ? guarded_clock([&] {
            return render_clock->GetPosition(&sample.position, &sample.qpc);
        }) : clock_service_hr);
        // Ordinary HRESULT failures can be transient: invalidate only this
        // sample and retry next time. Only a synthetic SEH failure disables the
        // optional method, avoiding repeated execution of a broken driver API.
        if ((static_cast<UINT32>(sample.position_hr) & 0xf0000000U) == 0xd0000000U) {
            clock_disabled = true; disabled_clock_hr = sample.position_hr;
        }
        const UINT64 observed = qpc100ns();
        const UINT64 difference = observed >= sample.qpc ? observed - sample.qpc : sample.qpc - observed;
        sample.valid = sample.position_hr == S_OK && clock_frequency_hr == S_OK && clock_frequency > 0 &&
                       sample.position > 0 && sample.qpc > 0 && difference <= 10 * 10000000ULL;
        return sample;
    };
    BYTE* preload = nullptr;
    hr = render->GetBuffer(target_padding, &preload);
    if (FAILED(hr) || !preload) return fail("error_PreloadGetBuffer", FAILED(hr) ? hr : E_POINTER);
    std::memset(preload, 0, static_cast<size_t>(target_padding) * block_align);
    hr = render->ReleaseBuffer(target_padding, AUDCLNT_BUFFERFLAGS_SILENT);
    if (FAILED(hr)) return fail("error_PreloadReleaseBuffer", hr);
    UINT64 written_frames = target_padding;
    Mapping prefill{}; prefill.event = "prefill_silence"; prefill.frames_written = target_padding;
    prefill.copy_before_qpc = qpc100ns(); prefill.copy_after_qpc = prefill.copy_before_qpc; write(prefill); csv.flush();
    RunningGuard running;
    hr = capture_client->Start();
    if (FAILED(hr)) return fail("error_StartCapture", hr);
    running.capture = capture_client.Get();
    hr = render_client->Start();
    if (FAILED(hr)) return fail("error_StartRender", hr);
    running.render = render_client.Get();
    const auto deadline = Steady::now() + std::chrono::seconds(seconds);
    std::deque<SourcePacket> source_queue;
    UINT32 queued_frames = 0;
    UINT64 packets_received = 0, copied_frames = 0, silence_frames = target_padding;
    auto next_flush = Steady::now() + std::chrono::seconds(1);
    const char* stop_reason = "stopped_duration";
    while (Steady::now() < deadline) {
        if (!stop_file.empty()) {
            std::error_code exists_error;
            if (std::filesystem::exists(stop_file, exists_error)) { stop_reason = "stopped_file"; break; }
            if (exists_error) return fail("error_CheckStopFile", HRESULT_FROM_WIN32(exists_error.value()));
        }
        UINT32 pending = 0;
        hr = capture->GetNextPacketSize(&pending);
        if (FAILED(hr)) return fail("error_GetNextCapturePacket", hr);
        while (pending > 0 && Steady::now() < deadline) {
            BYTE* source = nullptr;
            SourcePacket packet;
            hr = capture->GetBuffer(&source, &packet.total_frames, &packet.flags, &packet.device_frame, &packet.native_qpc);
            packet.received_qpc = qpc100ns();
            if (FAILED(hr)) return fail("error_GetCaptureBuffer", hr);
            if (packet.total_frames > rate || queued_frames > rate - packet.total_frames) {
                capture->ReleaseBuffer(packet.total_frames);
                return fail("error_SourceRingOverflow", HRESULT_FROM_WIN32(ERROR_BUFFER_OVERFLOW));
            }
            const bool silent = (packet.flags & AUDCLNT_BUFFERFLAGS_SILENT) != 0;
            if (packet.total_frames > 0 && !source && !silent) {
                capture->ReleaseBuffer(packet.total_frames);
                return fail("error_NullCaptureBuffer", E_POINTER);
            }
            packet.data.resize(static_cast<size_t>(packet.total_frames) * block_align);
            if (!silent && packet.total_frames > 0) std::memcpy(packet.data.data(), source, packet.data.size());
            hr = capture->ReleaseBuffer(packet.total_frames);
            if (FAILED(hr)) return fail("error_ReleaseCaptureBuffer", hr);
            if (packet.total_frames > 0) {
                Mapping received{}; received.event = "source_packet";
                received.source_device_frame = packet.device_frame; received.source_native_qpc = packet.native_qpc;
                received.source_received_qpc = packet.received_qpc; received.source_packet_frames = packet.total_frames;
                received.source_flags = packet.flags; received.renderer_written_frame = written_frames;
                queued_frames += packet.total_frames; received.queued_source_frames = queued_frames;
                write(received);
                source_queue.push_back(std::move(packet));
                ++packets_received;
            }
            hr = capture->GetNextPacketSize(&pending);
            if (FAILED(hr)) return fail("error_GetNextCapturePacket", hr);
        }
        UINT32 padding = 0;
        hr = render_client->GetCurrentPadding(&padding);
        if (FAILED(hr) || padding > render_buffer_frames) return fail("error_GetRenderPadding", FAILED(hr) ? hr : E_FAIL);
        if (padding < target_padding) {
            const UINT32 wanted = std::min(target_padding - padding, render_buffer_frames - padding);
            const ClockSample clock = read_clock();
            BYTE* destination = nullptr;
            hr = render->GetBuffer(wanted, &destination);
            if (FAILED(hr) || !destination) return fail("error_GetRenderBuffer", FAILED(hr) ? hr : E_POINTER);
            std::memset(destination, 0, static_cast<size_t>(wanted) * block_align);
            UINT32 filled = 0;
            std::vector<Mapping> mappings;
            while (filled < wanted && !source_queue.empty()) {
                SourcePacket& packet = source_queue.front();
                const UINT32 copied = std::min(wanted - filled, packet.total_frames - packet.consumed);
                Mapping row{};
                row.source_device_frame = packet.device_frame; row.source_native_qpc = packet.native_qpc;
                row.source_received_qpc = packet.received_qpc; row.source_packet_frames = packet.total_frames;
                row.source_offset_frames = packet.consumed; row.source_flags = packet.flags;
                row.renderer_written_frame = written_frames + filled; row.frames_written = copied;
                row.render_padding = padding; row.queued_source_frames = queued_frames; row.clock = clock;
                row.copy_before_qpc = qpc100ns();
                std::memcpy(destination + static_cast<size_t>(filled) * block_align,
                            packet.data.data() + static_cast<size_t>(packet.consumed) * block_align,
                            static_cast<size_t>(copied) * block_align);
                row.copy_after_qpc = qpc100ns();
                const double source_offset_seconds = (static_cast<double>(packet.consumed) - packet.total_frames) / rate;
                const INT64 source_estimate = static_cast<INT64>(packet.received_qpc) + static_cast<INT64>(std::llround(source_offset_seconds * 10000000.0));
                if (source_estimate > 0) row.source_start_estimated_qpc = static_cast<UINT64>(source_estimate);
                if (clock.valid && source_estimate > 0) {
                    const double until_present = static_cast<double>(row.renderer_written_frame) / rate -
                                                 static_cast<double>(clock.position) / clock.frequency;
                    const INT64 present = static_cast<INT64>(clock.qpc) + static_cast<INT64>(std::llround(until_present * 10000000.0));
                    if (present > 0) {
                        row.render_present_estimated_qpc = static_cast<UINT64>(present);
                        row.expected_delay_100ns = present - source_estimate;
                        row.expected_valid = true;
                    }
                }
                mappings.push_back(row);
                packet.consumed += copied; filled += copied; queued_frames -= copied; copied_frames += copied;
                if (packet.consumed == packet.total_frames) source_queue.pop_front();
            }
            if (filled < wanted) {
                Mapping row{}; row.event = "render_silence"; row.renderer_written_frame = written_frames + filled;
                row.frames_written = wanted - filled; row.render_padding = padding; row.queued_source_frames = queued_frames;
                row.clock = clock; row.copy_before_qpc = qpc100ns(); row.copy_after_qpc = row.copy_before_qpc;
                mappings.push_back(row); silence_frames += wanted - filled;
            }
            hr = render->ReleaseBuffer(wanted, filled == 0 ? AUDCLNT_BUFFERFLAGS_SILENT : 0);
            if (FAILED(hr)) return fail("error_ReleaseRenderBuffer", hr);
            for (const auto& row : mappings) write(row);
            written_frames += wanted;
        }
        if (Steady::now() >= next_flush) { csv.flush(); next_flush = Steady::now() + std::chrono::seconds(1); }
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
    hr = capture_client->Stop(); running.capture = nullptr;
    if (FAILED(hr)) return fail("error_StopCapture", hr);
    hr = render_client->Stop(); running.render = nullptr;
    if (FAILED(hr)) return fail("error_StopRender", hr);
    Mapping stopped{}; stopped.event = stop_reason; stopped.copy_before_qpc = qpc100ns();
    stopped.renderer_written_frame = written_frames; stopped.queued_source_frames = queued_frames; write(stopped); csv.flush();
    std::cout << "Reference finished: source_packets=" << packets_received << " copied_frames=" << copied_frames
              << " silence_frames=" << silence_frames << " queued_at_stop=" << queued_frames << '\n';
    return static_cast<bool>(csv);
}

int wmain(int argc, wchar_t** argv) {
    bool run = false, help = false;
    int seconds = 73;
    std::filesystem::path folder, stop_file;
    for (int i = 1; i < argc; ++i) {
        const std::wstring arg = argv[i];
        if (arg == L"--run") run = true;
        else if (arg == L"--help") help = true;
        else if (arg == L"--out" && i + 1 < argc) folder = argv[++i];
        else if (arg == L"--stop-file" && i + 1 < argc) stop_file = argv[++i];
        else if (arg == L"--seconds" && i + 1 < argc) {
            try { seconds = std::stoi(argv[++i]); }
            catch (...) { std::cerr << "Invalid seconds\n"; return 2; }
        } else { std::cerr << "Unknown or incomplete argument\n"; return 2; }
    }
    if (!run || help) {
        std::cout << "Real microphone to real renderer, memory-only reference.\n"
                     "mic-render-reference.exe --run --out ABS_DIR [--seconds 1..80] [--stop-file ABS_PATH]\n"
                     "No endpoint opens without --run. Default 73 seconds; hard watchdog 85 seconds.\n";
        return 0;
    }
    if (seconds < 1 || seconds > 80 || folder.empty() || !folder.is_absolute() ||
        (!stop_file.empty() && !stop_file.is_absolute())) {
        std::cerr << "Absolute --out/--stop-file and seconds 1..80 required\n"; return 2;
    }
    std::error_code error;
    if (!stop_file.empty() && std::filesystem::exists(stop_file, error)) {
        std::cout << "Stop file already exists; no endpoint opened\n"; return 0;
    }
    if (error) { std::cerr << "Cannot inspect stop file\n"; return 2; }
    std::filesystem::create_directories(folder, error);
    if (error) { std::cerr << "Cannot create output directory\n"; return 2; }
    std::atomic<bool> finished{false};
    std::thread watchdog([&] {
        const auto deadline = Steady::now() + std::chrono::seconds(85);
        while (!finished.load() && Steady::now() < deadline) std::this_thread::sleep_for(std::chrono::milliseconds(100));
        if (!finished.load()) { std::cerr << "Hard watchdog fired\n"; ExitProcess(124); }
    });
    bool success = false;
    try { success = run_reference(folder, stop_file, seconds); }
    catch (const std::exception& exception) { std::cerr << "Exception: " << exception.what() << std::endl; }
    finished = true; watchdog.join();
    return success ? 0 : 1;
}
