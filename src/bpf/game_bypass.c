#include <linux/bpf.h>
#include <linux/if_ether.h>
#include <linux/ip.h>
#include <linux/udp.h>

#ifndef SEC
#define SEC(NAME) __attribute__((section(NAME), used))
#endif

SEC("game_bypass")
int filter_game_ports(struct __sk_buff *skb) {
    void *data = (void *)(long)skb->data;
    void *data_end = (void *)(long)skb->data_end;

    struct ethhdr *eth = data;
    if ((void *)(eth + 1) > data_end)
        return 0;

    struct iphdr *ip = (void *)(eth + 1);
    if ((void *)(ip + 1) > data_end)
        return 0;

    if (ip->protocol != 17) // IPPROTO_UDP = 17
        return 0;

    struct udphdr *udp = (void *)(ip + 1);
    if ((void *)(udp + 1) > data_end)
        return 0;

    // Decode big-endian destination port
    unsigned short dest_port = ((udp->dest & 0xFF) << 8) | ((udp->dest >> 8) & 0xFF);

    // Moonlight (47998-48010) or Steam Link (27031-27036) ports
    if ((dest_port >= 47998 && dest_port <= 48010) || 
        (dest_port >= 27031 && dest_port <= 27036)) {
        
        // Direct queue priority bypass (Priority 6 is maps to Expedited class / DSCP EF)
        skb->priority = 6;
        return -1; // TC_ACT_UNSPEC: continue with priority applied
    }

    return 0;
}

char _license[] SEC("license") = "GPL";
