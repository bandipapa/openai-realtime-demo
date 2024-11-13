let ws = null;
let rtcPeer = null;

function connect() {
    if (ws)
        return;

    ws = new WebSocket("wss://" + window.location.host + "/ws");
    ws.addEventListener("open", connect_2);
    ws.addEventListener("message", ws_message);
    ws.addEventListener("error", () => { alert("ws: error"); });
    ws.addEventListener("close", () => { alert("ws: close"); });
}

async function connect_2() {
    const stream = await navigator.mediaDevices.getUserMedia({
        audio: true/*{//???
            autoGainControl: {
                ideal: true
            },
            echoCancellation: {
                ideal: true
            },
            noiseSuppression: {
                ideal: true
            }
        }*/
    });

    const config = {
        //iceServers: [{ urls: "stun:stun.l.google.com:19302" }]
    };
    rtcPeer = new RTCPeerConnection(config);

    rtcPeer.addEventListener("icecandidate", on_icecandidate);
    rtcPeer.addEventListener("track", on_track);

    for (const track of stream.getTracks())
        rtcPeer.addTrack(track, stream);

    const offer = await rtcPeer.createOffer();
    await rtcPeer.setLocalDescription(offer);

    const msg = {
        op: "INIT",
        data: offer.sdp
    }
    ws.send(JSON.stringify(msg));
}

function on_icecandidate(ev) {
    const candidate = ev.candidate;
    if (candidate) {
        const msg = {
            op: "ICE_CLIENT",
            data: JSON.stringify(candidate.toJSON())
        }
        ws.send(JSON.stringify(msg));    
    }
}

function on_track(ev) {
    const audio = document.getElementById("audio");
    audio.srcObject = ev.streams[0];
}

async function ws_message(pkt) {
    const msg = JSON.parse(pkt.data);

    switch (msg.op) {
        case "INIT_RESP":
            await rtcPeer.setRemoteDescription({type: "answer", sdp: msg.data});
            break;

        case "ICE_SERVER":
            await rtcPeer.addIceCandidate(JSON.parse(msg.data));
            break;

        case "ERR":
            alert("ws: " + msg.data);
            break;
    }
}
